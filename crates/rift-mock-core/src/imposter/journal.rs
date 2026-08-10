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

use super::types::{MatchOutcome, RecordedRequest};
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Cap on stored entries per port (issue #186); oldest evicted first.
pub const MAX_RECORDED_REQUESTS: usize = 10_000;

/// Result of reading a port's recorded requests.
pub struct JournalRead {
    pub entries: Vec<RecordedRequest>,
    /// `false` signals the backend could not reach all of its storage; callers serve the
    /// partial `entries` and surface the degradation rather than dropping data.
    pub complete: bool,
}

/// A recorded request paired with its stable per-port index (issue #603).
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub index: u64,
    pub request: RecordedRequest,
}

/// Result of a cursor read ([`RequestJournal::read_since`], issue #603).
#[derive(Debug, Clone)]
pub struct JournalReadSince {
    pub entries: Vec<JournalEntry>,
    /// Last index assigned for this port; `0` means nothing was ever recorded. Callers pass
    /// this back verbatim as the next `since`.
    ///
    /// This is the last *assigned* index, not the largest one still retained: it must not
    /// regress when entries leave the journal (a clear would otherwise hand out a cursor that
    /// moves backwards), and it must advance past entries the caller's filter rejected (a
    /// filtered tail would otherwise re-scan the same range forever).
    pub next: u64,
    /// Cap eviction discarded entries at or below the requested cursor, so the caller's view
    /// has a hole. Only retention pressure sets this — `clear`/`clear_flow`/`retain` are
    /// deliberate, caller-visible deletions and never do.
    ///
    /// A baseline read (`since: None`) is never truncated: it asks for whatever is retained
    /// and receives exactly that. `since: Some(0)` is a different question — "replay from the
    /// beginning" — and *is* truncated once anything has been evicted, because entries the
    /// caller asked for are gone. The two return identical entries but disagree here, and that
    /// is the intended distinction between snapshotting and resuming.
    pub truncated: bool,
    /// Same degraded-read semantics as [`JournalRead::complete`].
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

    /// Entries with `index > since` (all retained entries when `None`), keeping only those
    /// matching `keep` — applied *after* the cursor cut, over references before cloning so a
    /// `?match=` query does not deep-clone rejected entries (#485).
    ///
    /// `None` means this backend has no stable indices: callers fall back to
    /// [`Self::read_filtered`] and omit cursor metadata from responses, which is what lets an
    /// SDK probe for cursor support. Defaulting to `None` — rather than synthesizing indices
    /// from array offsets — is deliberate: offsets shift under eviction and scoped clears, so
    /// a synthetic cursor would silently skip or replay entries (issue #603).
    ///
    /// A cursor read that sets `complete: false` must not have its `next` trusted: callers
    /// withhold the cursor entirely rather than let a caller advance past entries a degraded
    /// backend could not serve.
    fn read_since(
        &self,
        port: u16,
        since: Option<u64>,
        keep: &dyn Fn(&RecordedRequest) -> bool,
    ) -> Option<JournalReadSince> {
        let _ = (port, since, keep);
        None
    }

    /// Like [`record`](Self::record), returning the index assigned to the entry; `None` when
    /// the backend does not support stable indices (issue #603).
    ///
    /// Override this and [`record`](Self::record) **both or neither**: the default delegates to
    /// `record`, so a backend that overrides only `record` and points it back here recurses
    /// forever. [`LocalJournal`] overrides both.
    fn record_indexed(&self, port: u16, flow_id: &str, req: RecordedRequest) -> Option<u64> {
        self.record(port, flow_id, req);
        None
    }

    /// Attach a match outcome to an entry already recorded under `index`.
    ///
    /// The outcome is only known after matching, which is after the entry exists — so this is a
    /// second write, not part of `record`. Two consequences are deliberate, and both are why this
    /// returns nothing:
    ///
    /// * A backend with no stable indices (one that leaves [`record_indexed`](Self::record_indexed)
    ///   at its default) has no way to address the entry, so the default here is a no-op. Such a
    ///   backend simply never carries outcomes; nothing else about it changes.
    /// * Attaching to an entry that is gone — evicted by the cap, or cleared between the record
    ///   and the match — is not an error. The entry the annotation describes no longer exists, so
    ///   there is nothing to say about it and nothing a caller could do.
    ///
    /// A diagnostic annotation must never be able to fail a request.
    fn attach_match(&self, port: u16, index: u64, outcome: MatchOutcome) {
        let _ = (port, index, outcome);
    }

    /// Attach the response status and the time taken to the entry `index` names (issue #364).
    ///
    /// A second write for the same reason [`attach_match`](Self::attach_match) is one: the entry is
    /// journalled before the request is matched, let alone answered, so neither the status nor the
    /// duration exists yet when `record` runs.
    ///
    /// Defaults to a no-op, and is infallible, on exactly the terms `attach_match` sets out — a
    /// backend without stable indices cannot address the entry, and an entry evicted between the
    /// record and the response is not an error. A diagnostic annotation must never be able to fail
    /// a request.
    fn attach_response(&self, port: u16, index: u64, status: u16, latency_ms: u64) {
        let _ = (port, index, status, latency_ms);
    }
}

#[derive(Default)]
struct PortSlot {
    /// Entries carrying their stable index (issue #603) and the flow id resolved at record
    /// time (issue #314: scoped clears must not re-derive flows from stored headers). A
    /// `VecDeque` so the oldest-first cap eviction is O(1) `pop_front` instead of an O(n)
    /// `Vec::remove(0)` shift (issue #289).
    entries: RwLock<VecDeque<(u64, String, RecordedRequest)>>,
    count: AtomicU64,
    /// Last index handed out for this port; 1-based, so 0 reads as "nothing recorded yet".
    /// Assigned under the `entries` write lock so index order always matches deque order.
    /// Never reset — not by eviction, not by `clear`/`clear_flow`/`retain` — which is what
    /// keeps a cursor held across any of them valid (issue #603).
    last_index: AtomicU64,
    /// Highest index dropped by *cap eviction*. Deliberate deletions never touch it: losing
    /// entries you asked to delete is not a hole in your view of the journal.
    evicted_through: AtomicU64,
    /// Whether the cap warning already fired for the current fill-up. A full journal evicts on
    /// EVERY record, and warning per eviction serialized the whole recording path on the
    /// tracing subscriber's writer at one log line per request (issue #718: −29% RPS at 200
    /// connections, −55% in the #702 sweep). Deliberate deletions re-arm it, so each fill-up
    /// warns exactly once.
    cap_warned: AtomicBool,
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
        self.record_indexed(port, flow_id, req);
    }

    fn record_indexed(&self, port: u16, flow_id: &str, req: RecordedRequest) -> Option<u64> {
        let slot = self.slot(port);
        // Allocated before taking the lock to keep the critical section minimal.
        let flow = flow_id.to_string();
        let mut entries = slot.entries.write();
        if entries.len() >= MAX_RECORDED_REQUESTS {
            if !slot.cap_warned.swap(true, Ordering::SeqCst) {
                tracing::warn!(
                    port,
                    max = MAX_RECORDED_REQUESTS,
                    "Recorded requests cap reached; evicting oldest entries (warned once per fill-up)"
                );
            }
            if let Some((evicted, _, _)) = entries.pop_front() {
                slot.evicted_through.store(evicted, Ordering::SeqCst);
            }
        }
        // Assigned under the write lock: a fetch_add outside it could interleave with a
        // concurrent recorder and push entries in a different order than their indices.
        let index = slot.last_index.fetch_add(1, Ordering::SeqCst) + 1;
        entries.push_back((index, flow, req));
        Some(index)
    }

    fn read(&self, port: u16) -> JournalRead {
        JournalRead {
            entries: self
                .slot(port)
                .entries
                .read()
                .iter()
                .map(|(_, _, req)| req.clone())
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
                .filter(|(_, _, req)| keep(req))
                .map(|(_, _, req)| req.clone())
                .collect(),
            complete: true,
        }
    }

    fn read_since(
        &self,
        port: u16,
        since: Option<u64>,
        keep: &dyn Fn(&RecordedRequest) -> bool,
    ) -> Option<JournalReadSince> {
        let slot = self.slot(port);
        let entries = slot.entries.read();
        // 0 admits every entry, since indices are 1-based — a baseline read needs no special case.
        let cut = since.unwrap_or(0);
        Some(JournalReadSince {
            entries: entries
                .iter()
                .filter(|(index, _, _)| *index > cut)
                .filter(|(_, _, req)| keep(req))
                .map(|(index, _, req)| JournalEntry {
                    index: *index,
                    request: req.clone(),
                })
                .collect(),
            next: slot.last_index.load(Ordering::SeqCst),
            // A baseline read sees everything retained, so it cannot have a hole.
            truncated: since.is_some_and(|s| slot.evicted_through.load(Ordering::SeqCst) > s),
            complete: true,
        })
    }

    fn clear(&self, port: u16) -> anyhow::Result<()> {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        entries.clear();
        slot.count.store(0, Ordering::SeqCst);
        // A deliberate deletion starts a new fill-up, which deserves its own cap warning.
        // Re-armed while still holding the write lock, like the swap in `record_indexed`, so
        // a racing recorder can never observe the stale flag and skip a fill-up's warning.
        slot.cap_warned.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        entries.retain(|(_, _, req)| keep(req));
        slot.cap_warned.store(false, Ordering::SeqCst);
    }

    fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        entries.retain(|(_, flow, _)| flow != flow_id);
        slot.cap_warned.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn count(&self, port: u16) -> u64 {
        self.slot(port).count.load(Ordering::SeqCst)
    }

    fn attach_match(&self, port: u16, index: u64, outcome: MatchOutcome) {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        // Indices are assigned under this same write lock, so the deque is always in strictly
        // ascending index order — a binary search is exact here, not merely a heuristic.
        if let Ok(position) = entries.binary_search_by(|(entry, _, _)| entry.cmp(&index)) {
            entries[position].2.match_outcome = Some(outcome);
        }
    }

    fn attach_response(&self, port: u16, index: u64, status: u16, latency_ms: u64) {
        let slot = self.slot(port);
        let mut entries = slot.entries.write();
        // Same exactness as `attach_match`: indices are assigned under this write lock, so the
        // deque is in strictly ascending index order and the search is precise, not heuristic.
        if let Ok(position) = entries.binary_search_by(|(entry, _, _)| entry.cmp(&index)) {
            entries[position].2.status = Some(status);
            entries[position].2.latency_ms = Some(latency_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::ResponseMode;

    fn req(path: &str) -> RecordedRequest {
        RecordedRequest {
            mode: ResponseMode::Text,
            request_from: "t".into(),
            method: "GET".into(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            body: None,
            timestamp: "t".into(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        }
    }

    /// Issue #364: the status and elapsed time land on the entry the index names, and only that one.
    #[test]
    fn attach_response_annotates_exactly_the_indexed_entry() {
        let j = LocalJournal::default();
        let first = j.record_indexed(1, "f", req("/a")).expect("indexed");
        let second = j.record_indexed(1, "f", req("/b")).expect("indexed");

        j.attach_response(1, second, 503, 42);

        let entries = j.read(1).entries;
        let a = entries
            .iter()
            .find(|r| r.path == "/a")
            .expect("/a retained");
        let b = entries
            .iter()
            .find(|r| r.path == "/b")
            .expect("/b retained");
        assert_eq!((b.status, b.latency_ms), (Some(503), Some(42)));
        // The neighbour is untouched — absent, not defaulted to a zero that would read as
        // "answered instantly with no status".
        assert_eq!((a.status, a.latency_ms), (None, None));
        assert_ne!(first, second);
    }

    /// Issue #364. Attaching to an entry that no longer exists is a no-op, never a panic: a
    /// diagnostic annotation must not be able to fail the request it describes.
    #[test]
    fn attach_response_to_a_missing_entry_is_a_no_op() {
        let j = LocalJournal::default();
        let index = j.record_indexed(1, "f", req("/a")).expect("indexed");
        j.clear(1).expect("clear");

        j.attach_response(1, index, 200, 1);
        j.attach_response(1, index + 9_999, 200, 1);

        assert!(j.read(1).entries.is_empty());
    }

    /// Issue #364. A journal with no stable indices cannot address an entry, so the trait default
    /// is a no-op — the backend simply never carries outcomes, and nothing else about it changes.
    #[test]
    fn a_backend_without_indices_ignores_the_attach() {
        struct NoIndices(RwLock<Vec<RecordedRequest>>);
        impl RequestJournal for NoIndices {
            fn note_request(&self, _port: u16) {}
            fn record(&self, _port: u16, _flow_id: &str, req: RecordedRequest) {
                self.0.write().push(req);
            }
            fn read(&self, _port: u16) -> JournalRead {
                JournalRead {
                    entries: self.0.read().clone(),
                    complete: true,
                }
            }
            fn clear(&self, _port: u16) -> anyhow::Result<()> {
                Ok(())
            }
            fn retain(&self, _port: u16, _keep: &dyn Fn(&RecordedRequest) -> bool) {}
            fn clear_flow(&self, _port: u16, _flow_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn count(&self, _port: u16) -> u64 {
                0
            }
        }

        let j = NoIndices(RwLock::new(Vec::new()));
        assert_eq!(
            j.record_indexed(1, "f", req("/a")),
            None,
            "no stable indices"
        );
        j.attach_response(1, 0, 500, 7);

        let entries = j.read(1).entries;
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].status, entries[0].latency_ms), (None, None));
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

    fn outcome() -> MatchOutcome {
        MatchOutcome {
            matched: false,
            stub_index: None,
            stub_id: None,
            tried: Vec::new(),
            tried_omitted: 0,
        }
    }

    // The outcome is attached AFTER the entry is recorded, so by the time it arrives the entry may
    // legitimately be gone (evicted by the cap, or the whole port never recorded). Both are no-ops:
    // a diagnostic annotation must never panic, and must never scribble on a neighbouring entry.
    #[test]
    fn attach_match_on_an_absent_entry_is_a_no_op() {
        let j = LocalJournal::default();
        for i in 0..(MAX_RECORDED_REQUESTS + 5) {
            j.record_indexed(1, "f", req(&format!("/{i}")));
        }
        // Indices 1..=5 fell off the front; 6 is the oldest retained.
        j.attach_match(1, 1, outcome());
        j.attach_match(9999, 1, outcome());

        let entries = j.read(1).entries;
        assert_eq!(entries.len(), MAX_RECORDED_REQUESTS);
        assert!(
            entries.iter().all(|r| r.match_outcome.is_none()),
            "attaching to an evicted index must not land on any surviving entry"
        );

        // Attaching to a live index still works, so the no-op above is about absence — not about
        // attach being inert everywhere.
        j.attach_match(1, 6, outcome());
        let entries = j.read(1).entries;
        assert!(entries[0].match_outcome.is_some(), "the addressed entry");
        assert!(
            entries[1..].iter().all(|r| r.match_outcome.is_none()),
            "and only that entry"
        );
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

    fn cursor(j: &LocalJournal, port: u16, since: Option<u64>) -> JournalReadSince {
        j.read_since(port, since, &|_| true)
            .expect("LocalJournal supports cursors")
    }

    fn indices(read: &JournalReadSince) -> Vec<u64> {
        read.entries.iter().map(|e| e.index).collect()
    }

    fn paths(read: &JournalReadSince) -> Vec<&str> {
        read.entries
            .iter()
            .map(|e| e.request.path.as_str())
            .collect()
    }

    // AC1 (#603): indices are 1-based and sequential; a baseline read returns every retained
    // entry with its index, and `next` is the last index assigned.
    #[test]
    fn cursor_assigns_1based_indices_and_reports_next() {
        let j = LocalJournal::default();
        assert_eq!(
            cursor(&j, 1, None).next,
            0,
            "an empty journal has assigned nothing; 0 is the 'seen nothing' cursor"
        );

        assert_eq!(j.record_indexed(1, "f", req("/a")), Some(1), "1-based");
        assert_eq!(j.record_indexed(1, "f", req("/b")), Some(2));
        assert_eq!(j.record_indexed(1, "f", req("/c")), Some(3));

        let read = cursor(&j, 1, None);
        assert_eq!(indices(&read), vec![1, 2, 3]);
        assert_eq!(paths(&read), vec!["/a", "/b", "/c"]);
        assert_eq!(read.next, 3, "next is the last assigned index");
        assert!(!read.truncated, "a baseline read can never be truncated");
        assert!(read.complete);

        // `record` shares the same counter — an unindexed write still advances the cursor.
        j.record(1, "f", req("/d"));
        assert_eq!(cursor(&j, 1, None).next, 4);

        // Indices are per-port, not global.
        assert_eq!(j.record_indexed(2, "f", req("/x")), Some(1));
    }

    // AC2 (#603): `since` is exclusive — strictly newer entries only; a cursor at or beyond the
    // last assigned index yields nothing but still reports the unchanged high-water mark.
    #[test]
    fn cursor_since_returns_strictly_newer() {
        let j = LocalJournal::default();
        for p in ["/a", "/b", "/c"] {
            j.record_indexed(1, "f", req(p));
        }

        let read = cursor(&j, 1, Some(1));
        assert_eq!(indices(&read), vec![2, 3], "index 1 was already seen");

        assert_eq!(indices(&cursor(&j, 1, Some(2))), vec![3]);

        let caught_up = cursor(&j, 1, Some(3));
        assert!(caught_up.entries.is_empty(), "nothing newer than the tip");
        assert_eq!(caught_up.next, 3, "next still reports the tip");

        let beyond = cursor(&j, 1, Some(99));
        assert!(
            beyond.entries.is_empty(),
            "a cursor beyond the tip is unambiguous — indices never reset"
        );
        assert_eq!(beyond.next, 3);
    }

    // AC3 (#603): `keep` is applied after the cursor cut, and composes with it.
    #[test]
    fn cursor_keep_composes_after_cut() {
        let j = LocalJournal::default();
        j.record_indexed(1, "f", req("/keep/1")); // 1
        j.record_indexed(1, "f", req("/drop/1")); // 2
        j.record_indexed(1, "f", req("/keep/2")); // 3
        j.record_indexed(1, "f", req("/drop/2")); // 4

        let keep_only = |r: &RecordedRequest| r.path.starts_with("/keep");

        let all = j.read_since(1, None, &keep_only).expect("cursor support");
        assert_eq!(indices(&all), vec![1, 3], "filtered, indices preserved");
        assert_eq!(all.next, 4, "next spans scanned entries, not returned ones");

        let after = j
            .read_since(1, Some(1), &keep_only)
            .expect("cursor support");
        assert_eq!(indices(&after), vec![3], "cut first, then filter");
        assert_eq!(after.next, 4);

        // A window whose entries all fail the filter must still advance the cursor, or a
        // filtered tail would re-scan the same range forever.
        let empty = j
            .read_since(1, Some(3), &|r| r.path == "/nothing")
            .expect("cursor support");
        assert!(empty.entries.is_empty());
        assert_eq!(empty.next, 4, "an all-rejected window still advances");
    }

    // AC4 (#603): a cursor held across a clear stays valid — indices continue, `next` does not
    // regress, and a deliberate clear is never reported as truncation.
    #[test]
    fn cursor_survives_clear_without_truncation() {
        let j = LocalJournal::default();
        j.record_indexed(1, "f", req("/a"));
        j.record_indexed(1, "f", req("/b"));
        j.clear(1).expect("local clear is infallible");

        let after_clear = cursor(&j, 1, Some(2));
        assert!(after_clear.entries.is_empty());
        assert_eq!(after_clear.next, 2, "next never regresses over a clear");
        assert!(
            !after_clear.truncated,
            "a clear is a deliberate deletion, not retention pressure"
        );

        j.record_indexed(1, "f", req("/c"));
        let resumed = cursor(&j, 1, Some(2));
        assert_eq!(
            indices(&resumed),
            vec![3],
            "post-clear entries keep counting up"
        );
        assert_eq!(paths(&resumed), vec!["/c"]);
        assert!(!resumed.truncated);

        // clear_flow and retain are deliberate too.
        j.record_indexed(1, "flow-x", req("/x"));
        j.clear_flow(1, "flow-x").expect("infallible");
        assert!(!cursor(&j, 1, Some(1)).truncated);
        j.retain(1, &|_| false);
        assert!(!cursor(&j, 1, Some(1)).truncated);
    }

    // AC5 (#603): `truncated` fires exactly when cap eviction discarded entries the cursor had
    // not yet seen.
    #[test]
    fn cursor_truncated_only_on_eviction_past_cursor() {
        let j = LocalJournal::default();
        for i in 0..(MAX_RECORDED_REQUESTS + 10) {
            j.record_indexed(1, "f", req(&format!("/{i}")));
        }
        // Indices 1..=10 were evicted; the oldest retained is 11.
        let read = cursor(&j, 1, None);
        assert_eq!(read.entries.len(), MAX_RECORDED_REQUESTS);
        assert_eq!(indices(&read)[0], 11, "oldest surviving index");
        assert_eq!(read.next, (MAX_RECORDED_REQUESTS + 10) as u64);

        assert!(
            cursor(&j, 1, Some(5)).truncated,
            "a cursor at 5 lost entries 6..=10 to the cap"
        );
        // The boundary sits exactly at the watermark: `since` is what the caller has already
        // seen, so a caller at 9 lost entry 10, while one at 10 lost nothing — everything it
        // asked for (11 onward) is still retained.
        assert!(
            cursor(&j, 1, Some(9)).truncated,
            "a cursor at 9 never received entry 10 before the cap took it"
        );
        assert!(
            !cursor(&j, 1, Some(10)).truncated,
            "a cursor at the watermark has seen everything eviction removed"
        );
        assert!(
            !cursor(&j, 1, Some(50)).truncated,
            "well ahead of the watermark"
        );
        assert!(
            !cursor(&j, 1, None).truncated,
            "a baseline read reads everything retained — no hole by definition"
        );
    }

    // AC5 (#603): `since: Some(0)` means "replay from the beginning" and reports the hole that
    // a baseline read — asking only for what is retained — cannot have. Same entries, different
    // question, and the difference is the whole point of the flag.
    #[test]
    fn cursor_since_zero_differs_from_baseline_only_in_truncation() {
        let j = LocalJournal::default();
        for i in 0..(MAX_RECORDED_REQUESTS + 3) {
            j.record_indexed(1, "f", req(&format!("/{i}")));
        }

        let baseline = cursor(&j, 1, None);
        let from_zero = cursor(&j, 1, Some(0));
        assert_eq!(
            indices(&baseline),
            indices(&from_zero),
            "identical entries: index > 0 admits everything retained"
        );
        assert_eq!(baseline.next, from_zero.next);
        assert!(!baseline.truncated, "a snapshot cannot have a hole");
        assert!(
            from_zero.truncated,
            "a replay from the beginning lost entries 1..=3 to the cap"
        );

        // Before any eviction the two agree completely.
        let fresh = LocalJournal::default();
        fresh.record_indexed(1, "f", req("/a"));
        assert!(!cursor(&fresh, 1, None).truncated);
        assert!(!cursor(&fresh, 1, Some(0)).truncated);
    }

    // AC1 (#603): indices are assigned under the entries write lock, so concurrent recorders can
    // never publish entries in a different order than their indices. Without that coupling a
    // reader could see the deque out of index order and `read_since`'s `index > cut` cut would
    // silently skip entries.
    #[test]
    fn cursor_indices_stay_ordered_under_concurrent_recorders() {
        use std::sync::Barrier;

        const RECORDERS: usize = 8;
        const PER_RECORDER: usize = 64;

        let j = Arc::new(LocalJournal::default());
        let barrier = Arc::new(Barrier::new(RECORDERS));
        let mut handles = Vec::new();
        for r in 0..RECORDERS {
            let j = Arc::clone(&j);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                (0..PER_RECORDER)
                    .filter_map(|i| j.record_indexed(1, "f", req(&format!("/{r}-{i}"))))
                    .collect::<Vec<_>>()
            }));
        }
        let mut assigned: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("recorder thread"))
            .collect();

        let total = (RECORDERS * PER_RECORDER) as u64;
        assigned.sort_unstable();
        assert_eq!(
            assigned,
            (1..=total).collect::<Vec<_>>(),
            "every index handed out exactly once, with no gaps"
        );

        let read = cursor(&j, 1, None);
        let seen = indices(&read);
        assert_eq!(seen.len(), total as usize);
        assert!(
            seen.windows(2).all(|w| w[0] < w[1]),
            "deque order must match index order, or the cursor cut skips entries"
        );
        assert_eq!(read.next, total);
    }

    // AC6 (#603): the trait defaults keep the extension non-breaking — a backend that does not
    // override them still records, and honestly reports that it has no cursor.
    #[test]
    fn default_journal_reports_no_cursor_support() {
        #[derive(Default)]
        struct DefaultJournal(LocalJournal);
        impl RequestJournal for DefaultJournal {
            fn note_request(&self, port: u16) {
                self.0.note_request(port);
            }
            fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
                self.0.record(port, flow_id, req);
            }
            fn read(&self, port: u16) -> JournalRead {
                self.0.read(port)
            }
            fn clear(&self, port: u16) -> anyhow::Result<()> {
                self.0.clear(port)
            }
            fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
                self.0.retain(port, keep);
            }
            fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
                self.0.clear_flow(port, flow_id)
            }
            fn count(&self, port: u16) -> u64 {
                self.0.count(port)
            }
        }

        let j = DefaultJournal::default();
        assert_eq!(
            j.record_indexed(1, "f", req("/a")),
            None,
            "no index to report"
        );
        assert_eq!(
            j.read(1).entries.len(),
            1,
            "but the request was still recorded"
        );
        assert!(
            j.read_since(1, None, &|_| true).is_none(),
            "absence of a cursor is reported honestly, not faked from offsets"
        );
    }

    // Issue #718: at steady-state-full every record evicts, and warning per eviction made the
    // tracing subscriber's writer serialize the whole recording path (−29% RPS at c=200 in the
    // attribution run; −55% in the #702 sweep harness). One fill-up warns exactly once.
    #[test]
    #[tracing_test::traced_test]
    fn cap_warns_once_per_fill_not_per_eviction() {
        let j = LocalJournal::default();
        for i in 0..(MAX_RECORDED_REQUESTS + 100) {
            j.record(1, "f", req(&format!("/{i}")));
        }
        logs_assert(|lines: &[&str]| {
            let n = lines.iter().filter(|l| l.contains("cap reached")).count();
            if n == 1 {
                Ok(())
            } else {
                Err(format!("expected exactly one cap warning, saw {n}"))
            }
        });
    }

    // Issue #718: deliberate deletions (clear / retain / clear_flow) start a new fill-up, so
    // the next cap hit warns again — but still only once per fill-up.
    #[test]
    #[tracing_test::traced_test]
    fn cap_warning_rearms_after_deliberate_deletions() {
        let j = LocalJournal::default();
        let fill = |j: &LocalJournal| {
            for i in 0..(MAX_RECORDED_REQUESTS + 5) {
                j.record(1, "f", req(&format!("/{i}")));
            }
        };
        fill(&j); // 5 evictions → 1 warning
        j.clear(1).expect("local clear is infallible");
        fill(&j); // re-armed by clear → 1 warning
        j.retain(1, &|_| false);
        fill(&j); // re-armed by retain → 1 warning
        j.clear_flow(1, "f")
            .expect("local clear_flow is infallible");
        fill(&j); // re-armed by clear_flow → 1 warning
        logs_assert(|lines: &[&str]| {
            let n = lines.iter().filter(|l| l.contains("cap reached")).count();
            if n == 4 {
                Ok(())
            } else {
                Err(format!(
                    "expected 4 cap warnings (one per fill-up), saw {n}"
                ))
            }
        });
    }

    // Issue #718: the warn-once flag lives on the port slot, so each port's fill-up gets its
    // own warning.
    #[test]
    #[tracing_test::traced_test]
    fn cap_warning_is_independent_per_port() {
        let j = LocalJournal::default();
        for port in [1u16, 2] {
            for i in 0..(MAX_RECORDED_REQUESTS + 5) {
                j.record(port, "f", req(&format!("/{i}")));
            }
        }
        logs_assert(|lines: &[&str]| {
            let n = lines.iter().filter(|l| l.contains("cap reached")).count();
            if n == 2 {
                Ok(())
            } else {
                Err(format!("expected 2 cap warnings (one per port), saw {n}"))
            }
        });
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
