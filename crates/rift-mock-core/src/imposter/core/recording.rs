//! Recorded-request storage: capture, retrieval, retention, and request counters.
//!
//! Thin wrappers over the imposter's `RequestJournal` (issue #314) — the public method
//! signatures are unchanged so the admin API and embedders are untouched.

use super::*;
use crate::imposter::journal::JournalRead;

impl Imposter {
    pub(super) fn journal_port(&self) -> u16 {
        self.config.port.unwrap_or(0)
    }

    /// Record a request (when body recording is enabled), tagged with its resolved flow id.
    pub fn record_request(&self, req: &RecordedRequest) {
        if self.config.record_requests {
            let flow_id = self.resolve_flow_id_recorded(&req.headers);
            self.journal
                .record(self.journal_port(), &flow_id, req.clone());
        }
    }

    /// Read recorded requests with the backend's completeness flag intact, so embedders
    /// can observe a degraded read programmatically (issue #314).
    pub fn read_recorded_requests(&self) -> JournalRead {
        self.journal.read(self.journal_port())
    }

    /// Get recorded requests. An incomplete read (backend partially unreachable) serves
    /// the partial entries and surfaces the degradation in the log; callers that need to
    /// react to it programmatically use [`Self::read_recorded_requests`].
    pub fn get_recorded_requests(&self) -> Vec<RecordedRequest> {
        let read = self.read_recorded_requests();
        if !read.complete {
            tracing::warn!(
                port = self.journal_port(),
                entries_served = read.entries.len(),
                "request journal returned an incomplete read; serving partial results"
            );
        }
        read.entries
    }

    /// Recorded requests matching `keep`, filtered over references before cloning so a `?match=`
    /// query does not deep-clone the whole journal just to discard most of it (issue #485).
    pub fn get_recorded_requests_filtered<F: Fn(&RecordedRequest) -> bool>(
        &self,
        keep: F,
    ) -> Vec<RecordedRequest> {
        let read = self.journal.read_filtered(self.journal_port(), &keep);
        if !read.complete {
            tracing::warn!(
                port = self.journal_port(),
                entries_served = read.entries.len(),
                "request journal returned an incomplete read; serving partial results"
            );
        }
        read.entries
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
