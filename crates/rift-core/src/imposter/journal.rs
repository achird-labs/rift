//! Pluggable recorded-request storage (issue #314): the trait extracted from the per-imposter
//! `RwLock<Vec<RecordedRequest>>` + `AtomicU64` counter, so embedders can substitute retention
//! policies, spill-to-disk, or external sinks.
//!
//! The default [`LocalJournal`] is behavior-identical to the embedded storage it replaces:
//! same 10k cap with oldest-first eviction, same count semantics (`clear` resets it,
//! `retain` does not). A journal injected via
//! [`ImposterManager::with_request_journal`](crate::imposter::ImposterManager::with_request_journal)
//! is shared across imposters and keyed by port; imposter deletion clears its port slice so
//! stale entries never resurrect on a later imposter reusing the port.

use super::types::RecordedRequest;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Cap on stored entries per port (issue #186); oldest evicted first.
pub const MAX_RECORDED_REQUESTS: usize = 10_000;

/// Result of reading a port's recorded requests.
pub struct JournalRead {
    pub entries: Vec<RecordedRequest>,
    /// `false` signals the backend could not reach all of its storage; callers serve the
    /// partial `entries` and surface the degradation rather than dropping data.
    pub complete: bool,
}

/// Pluggable recorded-request backend, keyed by imposter port.
pub trait RequestJournal: Send + Sync {
    /// Called for EVERY request (even when body recording is off) — backs
    /// `numberOfRequests`, matching the existing counter semantics.
    fn note_request(&self, port: u16);
    /// `flow_id` is the request's resolved flow (per the imposter's `flowIdSource`) —
    /// carried here so scoped clears don't have to re-derive it from stored headers.
    /// Record-time tags stay valid because `flowIdSource` is immutable per imposter;
    /// replacing an imposter clears its port slice.
    fn record(&self, port: u16, flow_id: &str, req: RecordedRequest);
    fn read(&self, port: u16) -> JournalRead;
    /// Clears entries AND resets the request count (documented contract, as today).
    ///
    /// Infallible by design: callers treat return as success, so an implementation whose
    /// backend fails must resolve the failure internally (retry/queue) and log it — never
    /// silently drop a failed clear.
    fn clear(&self, port: u16);
    /// Targeted deletion; does NOT reset the count (documented contract, as today).
    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool);
    /// Declarative scoped clear (one correlated slice, #223) — expressible by any
    /// backend, unlike the closure-based `retain`. Same failure contract as [`Self::clear`]:
    /// callers treat return as success.
    fn clear_flow(&self, port: u16, flow_id: &str);
    fn count(&self, port: u16) -> u64;
}

#[derive(Default)]
struct PortSlot {
    /// Entries paired with the flow id resolved at record time (issue #314: scoped clears
    /// must not re-derive flows from stored headers).
    entries: RwLock<Vec<(String, RecordedRequest)>>,
    count: AtomicU64,
}

/// Reference journal with the exact semantics of the storage it replaced.
#[derive(Default)]
pub struct LocalJournal {
    ports: RwLock<HashMap<u16, Arc<PortSlot>>>,
}

impl LocalJournal {
    fn slot(&self, port: u16) -> Arc<PortSlot> {
        if let Some(slot) = self.ports.read().get(&port) {
            return Arc::clone(slot);
        }
        Arc::clone(self.ports.write().entry(port).or_default())
    }
}

impl RequestJournal for LocalJournal {
    fn note_request(&self, port: u16) {
        self.slot(port).count.fetch_add(1, Ordering::SeqCst);
    }

    fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        if entries.len() >= MAX_RECORDED_REQUESTS {
            tracing::warn!(
                port,
                max = MAX_RECORDED_REQUESTS,
                "Recorded requests cap reached; oldest entry evicted"
            );
            entries.remove(0);
        }
        entries.push((flow_id.to_string(), req));
    }

    fn read(&self, port: u16) -> JournalRead {
        JournalRead {
            entries: self
                .slot(port)
                .entries
                .read()
                .iter()
                .map(|(_, req)| req.clone())
                .collect(),
            complete: true,
        }
    }

    fn clear(&self, port: u16) {
        let slot = self.slot(port);
        slot.entries.write().clear();
        slot.count.store(0, Ordering::SeqCst);
    }

    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
        self.slot(port).entries.write().retain(|(_, req)| keep(req));
    }

    fn clear_flow(&self, port: u16, flow_id: &str) {
        self.slot(port)
            .entries
            .write()
            .retain(|(flow, _)| flow != flow_id);
    }

    fn count(&self, port: u16) -> u64 {
        self.slot(port).count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str) -> RecordedRequest {
        RecordedRequest {
            request_from: "t".into(),
            method: "GET".into(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            body: None,
            timestamp: "t".into(),
        }
    }

    // AC1: the 10k cap evicts oldest-first, exactly like the embedded Vec did.
    #[test]
    fn local_caps_at_10k_evicting_oldest() {
        let j = LocalJournal::default();
        for i in 0..(MAX_RECORDED_REQUESTS + 10) {
            j.record(1, "f", req(&format!("/{i}")));
        }
        let read = j.read(1);
        assert_eq!(read.entries.len(), MAX_RECORDED_REQUESTS);
        assert_eq!(read.entries[0].path, "/10", "oldest entries evicted first");
        assert!(read.complete, "the local backend is always complete");
    }

    // AC1: note_request counts even when nothing is recorded (recording off).
    #[test]
    fn local_counts_without_recording() {
        let j = LocalJournal::default();
        j.note_request(1);
        j.note_request(1);
        assert_eq!(j.count(1), 2);
        assert!(j.read(1).entries.is_empty());
    }

    // AC1: clear drops entries AND resets the count (documented contract).
    #[test]
    fn local_clear_resets_count() {
        let j = LocalJournal::default();
        j.note_request(1);
        j.record(1, "f", req("/a"));
        j.clear(1);
        assert_eq!(j.count(1), 0);
        assert!(j.read(1).entries.is_empty());
    }

    // AC1: retain deletes targeted entries but does NOT reset the count.
    #[test]
    fn local_retain_preserves_count() {
        let j = LocalJournal::default();
        j.note_request(1);
        j.note_request(1);
        j.record(1, "f", req("/a"));
        j.record(1, "f", req("/b"));
        j.retain(1, &|r| r.path == "/b");
        assert_eq!(j.read(1).entries.len(), 1);
        assert_eq!(j.count(1), 2, "retain never resets the count");
    }

    // AC1: clear_flow removes exactly one correlated slice and keeps the count.
    #[test]
    fn local_clear_flow_removes_one_slice() {
        let j = LocalJournal::default();
        j.note_request(1);
        j.record(1, "flow-a", req("/a"));
        j.record(1, "flow-b", req("/b"));
        j.clear_flow(1, "flow-a");
        let read = j.read(1);
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].path, "/b");
        assert_eq!(j.count(1), 1, "scoped clear keeps the total count");
    }

    // Ports are isolated slices of the journal.
    #[test]
    fn local_ports_are_isolated() {
        let j = LocalJournal::default();
        j.record(1, "f", req("/a"));
        j.note_request(2);
        assert_eq!(j.read(1).entries.len(), 1);
        assert!(j.read(2).entries.is_empty());
        assert_eq!(j.count(1), 0);
        assert_eq!(j.count(2), 1);
    }
}
