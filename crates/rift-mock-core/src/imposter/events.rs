//! Admin event bus (issue #461): a broadcast of recorded-request and imposter-lifecycle events that
//! powers the admin SSE stream (`GET /events`). Owned by the [`ImposterManager`](super::ImposterManager);
//! publishing is a cheap no-op whenever nobody is subscribed (the SSE endpoint is the only
//! subscriber), so it adds nothing to the request hot path unless a client is actively streaming.
//!
//! Backpressure is lossy-but-loud: the channel is bounded, and a subscriber that falls behind
//! observes `RecvError::Lagged(n)` (surfaced to the client as a `lagged` SSE event) rather than the
//! engine blocking or buffering unboundedly — the client reconciles the gap via `GET /savedRequests`.

use crate::imposter::types::RecordedRequest;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

/// Broadcast capacity: a slow subscriber this far behind starts dropping oldest events (→ `Lagged`).
const CAPACITY: usize = 1024;

/// An imposter lifecycle transition. Mirrors [`ImposterEvent`](super::ImposterEvent) but is a
/// distinct type so the SSE wire format is not coupled to the embedder-facing event enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImposterAction {
    Created,
    Replaced,
    StubsChanged,
    Deleted,
    AllDeleted,
}

impl ImposterAction {
    /// The `action` string in the SSE `imposter` event payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::StubsChanged => "stubsChanged",
            Self::Deleted => "deleted",
            Self::AllDeleted => "allDeleted",
        }
    }
}

/// A single admin event, tagged with a monotonic sequence number (the SSE `id:`), so a client that
/// reconnects can tell — via `Last-Event-ID` — that it missed events (v1 does not replay; the gap
/// is the signal to reconcile via polling).
#[derive(Debug, Clone)]
pub struct AdminEvent {
    pub seq: u64,
    pub kind: AdminEventKind,
}

/// The two event families the stream carries.
#[derive(Debug, Clone)]
pub enum AdminEventKind {
    /// An imposter was created/replaced/deleted (`port` is absent only for `AllDeleted`).
    Lifecycle {
        action: ImposterAction,
        port: Option<u16>,
    },
    /// A request was recorded against `port` (only when `recordRequests: true`), tagged with its
    /// resolved `flow_id`. `request` is byte-identical to the `savedRequests` projection.
    ///
    /// `index` is the entry's journal index (issue #603), or `None` when the backend has no
    /// stable indices. It lets a client that lagged or reconnected reconcile with
    /// `?since=<index>` instead of re-polling the whole journal.
    ///
    /// The request is boxed because a broadcast ring slot is sized by the largest variant: left
    /// inline, every one of the bus's 1024 slots would carry a `RecordedRequest`'s worth of
    /// bytes even when it holds a ~5-byte lifecycle event.
    Request {
        port: u16,
        flow_id: String,
        index: Option<u64>,
        request: Box<RecordedRequest>,
    },
}

/// Broadcast of admin events for the SSE stream.
#[derive(Debug)]
pub struct AdminEventBus {
    tx: broadcast::Sender<Arc<AdminEvent>>,
    seq: AtomicU64,
    /// Serializes (sequence assignment + broadcast send) so that `id:` delivery order always equals
    /// assignment order — `publish` is called concurrently from lifecycle (`emit`) and per-request
    /// (`record_request`) paths, and without this a later `seq` could be `send`-ed first, delivering
    /// events out of order and breaking the monotonic-`id:` reconnect contract.
    send_lock: parking_lot::Mutex<()>,
}

impl Default for AdminEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminEventBus {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self {
            tx,
            seq: AtomicU64::new(0),
            send_lock: parking_lot::Mutex::new(()),
        }
    }

    /// The most recently issued sequence number (0 before any event) — sent in the `hello` frame so
    /// a client knows where the stream starts.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// Subscribe a new SSE stream. Each subscriber gets every event published after this call.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AdminEvent>> {
        self.tx.subscribe()
    }

    /// True when at least one SSE stream is subscribed — the publish-side fast-path guard that keeps
    /// the request hot path allocation-free when nobody is streaming.
    #[must_use]
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Publish an imposter lifecycle event (called from `ImposterManager::emit`).
    pub fn publish_lifecycle(&self, action: ImposterAction, port: Option<u16>) {
        self.publish(AdminEventKind::Lifecycle { action, port });
    }

    /// Publish a recorded-request event (called from `Imposter::record_request`). The caller checks
    /// [`has_subscribers`](Self::has_subscribers) first so it never clones a request for nobody.
    pub fn publish_request(
        &self,
        port: u16,
        flow_id: String,
        index: Option<u64>,
        request: RecordedRequest,
    ) {
        self.publish(AdminEventKind::Request {
            port,
            flow_id,
            index,
            request: Box::new(request),
        });
    }

    fn publish(&self, kind: AdminEventKind) {
        // No SSE client: don't burn a sequence number or a broadcast slot.
        if self.tx.receiver_count() == 0 {
            return;
        }
        // Assign the sequence and send under one lock so `id:` order == delivery order even under
        // concurrent publishers. `send` is non-blocking (no `.await`), so this is a brief hold.
        let _guard = self.send_lock.lock();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        // A send error means every receiver dropped between the check and here — nothing to do.
        let _ = self.tx.send(Arc::new(AdminEvent { seq, kind }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::ResponseMode;

    fn rec() -> RecordedRequest {
        RecordedRequest {
            mode: ResponseMode::Text,
            request_from: "127.0.0.1:5000".to_string(),
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
            headers: Default::default(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn no_subscribers_means_no_publish() {
        // AC9: with nobody streaming, publishing must not advance the sequence (the hot-path guard).
        let bus = AdminEventBus::new();
        assert!(!bus.has_subscribers());
        bus.publish_lifecycle(ImposterAction::Created, Some(1));
        bus.publish_request(1, "f".to_string(), Some(1), rec());
        assert_eq!(bus.seq(), 0, "no subscribers → no sequence consumed");
    }

    #[tokio::test]
    async fn subscriber_receives_and_seq_advances() {
        let bus = AdminEventBus::new();
        let mut rx = bus.subscribe();
        assert!(bus.has_subscribers());
        bus.publish_lifecycle(ImposterAction::Deleted, Some(7));
        let ev = rx.recv().await.expect("event");
        assert_eq!(ev.seq, 1);
        assert!(matches!(
            ev.kind,
            AdminEventKind::Lifecycle {
                action: ImposterAction::Deleted,
                port: Some(7)
            }
        ));
        assert_eq!(bus.seq(), 1);
    }
}
