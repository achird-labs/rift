//! Recorded-request storage: capture, retrieval, retention, and request counters.
//!
//! Thin wrappers over the imposter's `RequestJournal` (issue #314) — the public method
//! signatures are unchanged so the admin API and embedders are untouched.

use super::*;
use crate::imposter::journal::{JournalRead, JournalReadSince};

impl Imposter {
    pub(super) fn journal_port(&self) -> u16 {
        self.config.port.unwrap_or(0)
    }

    /// Record a request (when body recording is enabled), tagged with its resolved flow id.
    ///
    /// Returns the journal index assigned to the entry, or `None` when nothing was recorded
    /// (recording is off) or the backend has no stable indices (issue #603).
    pub fn record_request(&self, req: &RecordedRequest) -> Option<u64> {
        if !self.config.record_requests {
            return None;
        }
        let flow_id = self.resolve_flow_id_recorded(&req.headers);
        self.journal
            .record_indexed(self.journal_port(), &flow_id, req.clone())
    }

    /// Read recorded requests with the backend's completeness flag intact, so embedders
    /// can observe a degraded read programmatically (issue #314).
    pub fn read_recorded_requests(&self) -> JournalRead {
        self.journal.read(self.journal_port())
    }

    fn warn_if_incomplete(&self, complete: bool, entries_served: usize) {
        if !complete {
            tracing::warn!(
                port = self.journal_port(),
                entries_served,
                "request journal returned an incomplete read; serving partial results"
            );
        }
    }

    /// Get recorded requests. An incomplete read (backend partially unreachable) serves
    /// the partial entries and surfaces the degradation in the log; callers that need to
    /// react to it programmatically use [`Self::read_recorded_requests`].
    pub fn get_recorded_requests(&self) -> Vec<RecordedRequest> {
        let read = self.read_recorded_requests();
        self.warn_if_incomplete(read.complete, read.entries.len());
        read.entries
    }

    /// Recorded requests matching `keep`, filtered over references before cloning so a `?match=`
    /// query does not deep-clone the whole journal just to discard most of it (issue #485).
    pub fn get_recorded_requests_filtered<F: Fn(&RecordedRequest) -> bool>(
        &self,
        keep: F,
    ) -> Vec<RecordedRequest> {
        let read = self.journal.read_filtered(self.journal_port(), &keep);
        self.warn_if_incomplete(read.complete, read.entries.len());
        read.entries
    }

    /// Recorded requests newer than `since` (all retained when `None`), matching `keep`.
    /// `None` means the backend has no stable indices — callers fall back to
    /// [`Self::get_recorded_requests_filtered`] and omit cursor metadata (issue #603).
    ///
    /// A returned read with `complete: false` has a `next` spanning entries it could not
    /// serve: callers must withhold the cursor rather than let it advance past them.
    pub fn read_recorded_requests_since<F: Fn(&RecordedRequest) -> bool>(
        &self,
        since: Option<u64>,
        keep: F,
    ) -> Option<JournalReadSince> {
        let read = self.journal.read_since(self.journal_port(), since, &keep)?;
        self.warn_if_incomplete(read.complete, read.entries.len());
        Some(read)
    }

    /// Clear recorded requests. Also resets the request count to match Mountebank behavior.
    /// Propagates a backend clear failure (issue #330) so callers never report a clean clear
    /// over stale recorded state.
    pub fn clear_recorded_requests(&self) -> anyhow::Result<()> {
        self.journal.clear(self.journal_port())
    }

    /// Retain only the recorded requests for which `keep` returns true.
    /// Used for targeted clears (a single correlated slice); unlike
    /// `clear_recorded_requests` it does not reset the total request count,
    /// since other slices' requests remain.
    pub fn retain_recorded_requests<F: Fn(&RecordedRequest) -> bool>(&self, keep: F) {
        self.journal.retain(self.journal_port(), &keep);
    }

    /// Clear saved proxy responses
    pub fn clear_proxy_responses(&self) {
        self.proxy_store.clear(self.journal_port());
    }

    /// Count this request toward `numberOfRequests` (fires even when recording is off).
    pub fn increment_request_count(&self) {
        self.journal.note_request(self.journal_port());
    }

    /// Get request count
    pub fn get_request_count(&self) -> u64 {
        self.journal.count(self.journal_port())
    }
}
