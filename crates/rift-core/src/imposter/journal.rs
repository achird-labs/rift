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
use std::collections::{HashMap, VecDeque};
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
    /// Like [`read`](Self::read) but keeps only entries matching `keep`. The default filters the
    /// full read; backends should override to filter over references BEFORE cloning, so a
    /// `?match=` query does not deep-clone the whole journal just to discard most of it (#485).
    fn read_filtered(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) -> JournalRead {
        let read = self.read(port);
        JournalRead {
            entries: read.entries.into_iter().filter(|r| keep(r)).collect(),
            complete: read.complete,
        }
    }
    /// Clears entries AND resets the request count (documented contract, as today).
    ///
    /// Fallible: clearing is a correctness operation whose postcondition ("the data is gone")
    /// callers rely on (issue #330). A backend that cannot delete must return `Err` so callers
    /// surface the degradation rather than reporting a clean clear over stale state.
    fn clear(&self, port: u16) -> anyhow::Result<()>;
    /// Targeted deletion; does NOT reset the count (documented contract, as today).
    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool);
    /// Declarative scoped clear (one correlated slice, #223) — expressible by any
    /// backend, unlike the closure-based `retain`. Fallible for the same reason as
    /// [`Self::clear`]: a failed delete must surface, not be reported as success.
    fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()>;
    fn count(&self, port: u16) -> u64;
}

#[derive(Default)]
struct PortSlot {
    /// Entries paired with the flow id resolved at record time (issue #314: scoped clears
    /// must not re-derive flows from stored headers). A `VecDeque` so the oldest-first cap
    /// eviction is O(1) `pop_front` instead of an O(n) `Vec::remove(0)` shift (issue #289).
    entries: RwLock<VecDeque<(String, RecordedRequest)>>,
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
            entries.pop_front();
        }
        entries.push_back((flow_id.to_string(), req));
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

    fn read_filtered(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) -> JournalRead {
        // Filter over references under the read lock so only matching entries are cloned (#485).
        JournalRead {
            entries: self
                .slot(port)
                .entries
                .read()
                .iter()
                .filter(|(_, req)| keep(req))
                .map(|(_, req)| req.clone())
                .collect(),
            complete: true,
        }
    }

    fn clear(&self, port: u16) -> anyhow::Result<()> {
        let slot = self.slot(port);
        slot.entries.write().clear();
        slot.count.store(0, Ordering::SeqCst);
        Ok(())
    }

    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
        self.slot(port).entries.write().retain(|(_, req)| keep(req));
    }

    fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
        self.slot(port)
            .entries
            .write()
            .retain(|(flow, _)| flow != flow_id);
        Ok(())
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

    // Issue #485: read_filtered returns only entries matching the predicate (same result as
    // read().filter, but the LocalJournal override clones only the matches).
    #[test]
    fn local_read_filtered_keeps_only_matches() {
        let j = LocalJournal::default();
        j.record(1, "f", req("/keep/1"));
        j.record(1, "f", req("/drop/1"));
        j.record(1, "f", req("/keep/2"));

        let read = j.read_filtered(1, &|r| r.path.starts_with("/keep"));
        let paths: Vec<&str> = read.entries.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["/keep/1", "/keep/2"]);
        assert!(read.complete);

        // Parity with read().filter(...) over the same predicate.
        let via_read: Vec<String> = j
            .read(1)
            .entries
            .into_iter()
            .filter(|r| r.path.starts_with("/keep"))
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, via_read);

        // An always-false predicate yields nothing; always-true yields all.
        assert!(j.read_filtered(1, &|_| false).entries.is_empty());
        assert_eq!(j.read_filtered(1, &|_| true).entries.len(), 3);
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
        j.clear(1).expect("local clear is infallible");
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
        j.clear_flow(1, "flow-a")
            .expect("local clear_flow is infallible");
        let read = j.read(1);
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].path, "/b");
        assert_eq!(j.count(1), 1, "scoped clear keeps the total count");
    }

    // AC1 (#330): the local backend never fails a clear — both clear ops return Ok.
    #[test]
    fn local_clear_and_clear_flow_are_ok() {
        let j = LocalJournal::default();
        j.record(1, "flow-a", req("/a"));
        assert!(j.clear_flow(1, "flow-a").is_ok());
        assert!(j.clear(1).is_ok());
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
