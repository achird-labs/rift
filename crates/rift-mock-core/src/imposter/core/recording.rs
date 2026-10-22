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
    /// Takes `req` by value (issue #561): the caller's owned `RecordedRequest` is never used
    /// again afterward, so this used to pay a full clone (up to 10 MB) just to satisfy the
    /// by-value `RequestJournal::record`. It is now moved into the journal directly, and cloned
    /// only in the (uncommon) case an SSE subscriber also needs its own copy.
    ///
    /// Returns the journal index assigned to the entry, or `None` when nothing was recorded
    /// (recording is off) or the backend has no stable indices (issue #603).
    pub fn record_request(&self, req: RecordedRequest) -> Option<u64> {
        if !self.config.record_requests {
            return None;
        }
        let flow_id = self.resolve_flow_id_recorded(&req.headers);
        // Fan the recorded request out to the admin SSE stream (issue #461). Guard on an active
        // subscriber first so the hot path never clones the request for nobody. The journal
        // index rides along (issue #603) so a client that reconnects or lags can reconcile with
        // `?since=<index>` instead of re-polling the whole journal.
        match self.event_bus.as_ref().filter(|bus| bus.has_subscribers()) {
            Some(bus) => {
                // A subscriber needs its own copy after this call, so clone once for the journal
                // and move the original into `publish_request`.
                let index = self
                    .journal
                    .record_indexed(self.journal_port(), &flow_id, req.clone());
                bus.publish_request(self.journal_port(), flow_id, index, req);
                index
            }
            // Nobody is listening: move `req` straight into the journal, no clone at all.
            None => self
                .journal
                .record_indexed(self.journal_port(), &flow_id, req),
        }
    }

    /// Attach a match outcome to the entry [`record_request`](Self::record_request) returned
    /// `index` for, once this request's matching pass has finished.
    ///
    /// Infallible by design: see [`RequestJournal::attach_match`](crate::imposter::RequestJournal::attach_match)
    /// for why a backend without indices, or an entry already evicted, is a no-op rather than an
    /// error. Nothing about the response depends on it.
    pub fn attach_match_outcome(&self, index: u64, outcome: MatchOutcome) {
        self.journal
            .attach_match(self.journal_port(), index, outcome);
    }

    /// Attach the response status and elapsed time to the entry [`record_request`](Self::record_request)
    /// returned `index` for, once the response has been produced (issue #364).
    ///
    /// Infallible on the same terms as [`attach_match_outcome`](Self::attach_match_outcome), and
    /// for the same reason: a journal annotation that could fail a request would trade a real
    /// response for a diagnostic.
    pub fn attach_response_outcome(&self, index: u64, status: u16, latency_ms: u64) {
        self.journal
            .attach_response(self.journal_port(), index, status, latency_ms);
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
    ///
    /// With `--datadir`, `{port}.requests.jsonl` is removed first (issue #1608); one that cannot
    /// be removed fails the clear with the requests still recorded.
    pub fn clear_recorded_requests(&self) -> anyhow::Result<()> {
        let clear = || self.journal.clear(self.journal_port());
        match &self.journal_file {
            Some(file) => file.clear(self.persists_journal(), clear),
            None => clear(),
        }
    }

    /// Whether this imposter's journal is written to its datadir (issue #1608).
    fn persists_journal(&self) -> bool {
        self.persistence() == crate::imposter::Persistence::Datadir
    }

    /// Run a scoped deletion on the journal and, with `--datadir`, rewrite `{port}.requests.jsonl`
    /// from what it kept (issue #1608), so a restart does not bring the deleted entries back.
    pub(super) fn delete_recorded<R>(
        &self,
        delete: impl FnOnce() -> R,
    ) -> (R, std::io::Result<()>) {
        match &self.journal_file {
            Some(file) => file.deleting(self.persists_journal(), delete),
            None => (delete(), Ok(())),
        }
    }

    /// Retain only the recorded requests for which `keep` returns true.
    /// Used for targeted clears (a single correlated slice); unlike
    /// `clear_recorded_requests` it does not reset the total request count,
    /// since other slices' requests remain.
    ///
    /// With `--datadir` the file is rewritten from what is kept (issue #1608). A rewrite that fails
    /// is logged at error level and retried by the next flush; until then a restart could bring
    /// back what this removed.
    pub fn retain_recorded_requests<F: Fn(&RecordedRequest) -> bool>(&self, keep: F) {
        let ((), written) =
            self.delete_recorded(|| self.journal.retain(self.journal_port(), &keep));
        if let Err(e) = written {
            tracing::error!(
                port = self.journal_port(),
                "could not rewrite the recorded requests file after a scoped clear: {e}"
            );
        }
    }

    /// Clear saved proxy responses, as Mountebank's `deleteSavedProxyResponses` does: the proxy
    /// store, and every response a proxy recorded (its `is` carries a numeric
    /// `_proxyResponseTime`), including those a `proxyAlways` proxy appended to a stub the author
    /// wrote (issue #1519). A stub this leaves with no responses is removed, and every remaining
    /// stub starts its response cycle over. Recorded requests are kept.
    ///
    /// A stub recorded by an engine from before the mark (`recordedFrom`, no marked response) is
    /// removed whole (issue #1239), so a datadir or a replayed log from then is cleared too.
    ///
    /// An embedder should prefer
    /// [`ImposterManager::clear_proxy_responses`](crate::imposter::ImposterManager::clear_proxy_responses),
    /// which also restarts a sequencer's cursors, and announces and persists a changed stub set.
    pub fn clear_proxy_responses(&self) -> ClearedProxyResponses {
        self.proxy_store.clear(self.journal_port());
        // One mutation for the filter and the overwrite, so a recording racing the clear is either
        // cleared or kept, never lost between a read and a write.
        self.mutate_stubs(|stubs| {
            let mut changed = false;
            let kept: Vec<Stub> = stubs
                .iter()
                .filter_map(|state| {
                    let stub = &state.stub;
                    let marked = stub.responses.iter().any(StubResponse::is_recorded);
                    if !marked {
                        if stub.recorded_from.is_some() {
                            changed = true;
                            return None;
                        }
                        return Some(stub.clone());
                    }
                    changed = true;
                    let mut stripped = stub.clone();
                    stripped.responses.retain(|r| !r.is_recorded());
                    (!stripped.responses.is_empty()).then_some(stripped)
                })
                .collect();
            self.overwrite_all(stubs, kept);
            ClearedProxyResponses { changed }
        })
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

/// What [`Imposter::clear_proxy_responses`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct ClearedProxyResponses {
    /// A stub was removed or lost a response. A clear always restarts every cycle, but only this
    /// changes the config.
    pub changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TO: &str = "http://127.0.0.1:1";

    fn imposter(stubs: serde_json::Value) -> Imposter {
        let config =
            serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": stubs }))
                .expect("config");
        Imposter::new(config).expect("imposter")
    }

    fn proxy_stub() -> serde_json::Value {
        json!({ "responses": [{ "proxy": { "to": TO, "mode": "proxyAlways" } }] })
    }

    /// Each stub's response bodies, in display order.
    fn bodies(imposter: &Imposter) -> Vec<Vec<String>> {
        imposter
            .get_stubs()
            .iter()
            .map(|stub| {
                stub.responses
                    .iter()
                    .map(
                        |r| match serde_json::to_value(r).expect("prints").pointer("/is/body") {
                            Some(serde_json::Value::String(body)) => body.clone(),
                            _ => "<not is>".to_string(),
                        },
                    )
                    .collect()
            })
            .collect()
    }

    fn next_body(imposter: &Imposter, index: usize) -> String {
        let snapshot = imposter.snapshot();
        let response = snapshot.stubs()[index]
            .get_next_response()
            .expect("stub has responses");
        serde_json::to_value(response).expect("prints")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    #[test]
    fn an_appended_response_is_stripped_and_the_authored_ones_stay() {
        let on_b = json!([{ "deepEquals": { "path": "/b" } }]);
        let imposter = imposter(json!([
            proxy_stub(),
            { "predicates": on_b, "responses": [{ "is": { "body": "A" } }, { "is": { "body": "B" } }] }
        ]));
        let recorded: Stub = serde_json::from_value(json!({
            "predicates": on_b, "recordedFrom": TO,
            "responses": [{ "is": { "body": "U", "_proxyResponseTime": 7 } }]
        }))
        .expect("stub");
        imposter.insert_or_append_proxy_stub(recorded, TO, "proxyAlways", None, None);
        assert_eq!(bodies(&imposter)[1], ["A", "B", "U"]);

        assert!(imposter.clear_proxy_responses().changed);
        assert_eq!(
            bodies(&imposter),
            vec![vec!["<not is>".to_string()], vec!["A".into(), "B".into()]]
        );
    }

    #[test]
    fn a_marked_stub_goes_whole_and_a_partly_marked_one_keeps_the_rest() {
        let imposter = imposter(json!([
            { "predicates": [{ "equals": { "path": "/i" } }],
              "responses": [{ "is": { "body": "IMP", "_proxyResponseTime": 5 } }] },
            { "predicates": [{ "equals": { "path": "/m" } }],
              "responses": [{ "is": { "body": "M1" } }, { "is": { "body": "M2", "_proxyResponseTime": 3 } }] },
            { "predicates": [{ "equals": { "path": "/n" } }], "responses": [{ "is": { "body": "N" } }] }
        ]));
        assert!(imposter.clear_proxy_responses().changed);
        assert_eq!(
            bodies(&imposter),
            vec![vec!["M1".to_string()], vec!["N".to_string()]]
        );
    }

    #[test]
    fn a_legacy_recording_without_marks_goes_whole() {
        let imposter = imposter(json!([
            { "recordedFrom": TO, "responses": [{ "is": { "body": "old" } }] },
            { "responses": [{ "is": { "body": "mine" } }] }
        ]));
        assert!(imposter.clear_proxy_responses().changed);
        assert_eq!(bodies(&imposter), vec![vec!["mine".to_string()]]);
    }

    /// Config refuses `responses: []`, but an embedder can build such a stub; nothing was removed
    /// from it, so the clear keeps it.
    #[test]
    fn a_stub_built_with_no_responses_is_kept() {
        let imposter = imposter(json!([]));
        let mut empty: Stub =
            serde_json::from_value(json!({ "responses": [{ "is": { "body": "x" } }] }))
                .expect("stub");
        let other = empty.clone();
        empty.responses.clear();
        imposter.replace_stubs(vec![empty, other]);
        assert!(!imposter.clear_proxy_responses().changed);
        assert_eq!(imposter.get_stubs().len(), 2);
    }

    #[test]
    fn clearing_with_nothing_recorded_restarts_every_cycle() {
        let imposter = imposter(json!([
            { "responses": [{ "is": { "body": "A" } }, { "is": { "body": "B" } }, { "is": { "body": "C" } }] }
        ]));
        assert_eq!(
            [next_body(&imposter, 0), next_body(&imposter, 0)],
            ["A", "B"]
        );
        assert!(!imposter.clear_proxy_responses().changed);
        assert_eq!(next_body(&imposter, 0), "A", "the cycle restarted");
    }

    // Issue #1303: stamps are aligned by position, as a bulk replace aligns them.
    #[test]
    fn an_untouched_prefix_keeps_its_stamp_and_a_stub_behind_a_removal_takes_a_new_one() {
        let imposter = imposter(json!([
            { "predicates": [{ "equals": { "path": "/a" } }], "responses": [{ "is": { "body": "a" } }] },
            { "predicates": [{ "equals": { "path": "/r" } }],
              "responses": [{ "is": { "body": "r", "_proxyResponseTime": 1 } }] },
            { "predicates": [{ "equals": { "path": "/c" } }], "responses": [{ "is": { "body": "c" } }] }
        ]));
        let before: Vec<_> = imposter
            .snapshot()
            .stubs()
            .iter()
            .map(|s| s.loaded_at())
            .collect();
        assert!(imposter.clear_proxy_responses().changed);
        let after: Vec<_> = imposter
            .snapshot()
            .stubs()
            .iter()
            .map(|s| s.loaded_at())
            .collect();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0], "the prefix is untouched");
        assert!(
            after[1] > before[2],
            "the stub behind the removal is restamped"
        );
    }
}
