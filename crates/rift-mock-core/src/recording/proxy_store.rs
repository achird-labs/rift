//! Pluggable proxy-recording backend (issue #315): the trait extracted from the concrete
//! per-imposter in-memory store, so embedders can persist
//! recordings or share the `proxyOnce` exactly-once gate across engine instances.
//!
//! The default [`LocalProxyStore`] kept the behaviour of the earlier embedded store (removed with
//! the reverse-proxy mode in #975) —
//! same `proxyOnce`/`proxyAlways`/`proxyTransparent` semantics, the same TOCTOU fix
//! (#171/#118), and the same caps — **plus** a release-on-error fix: a claim taken by
//! [`try_claim`](ProxyRecordingStore::try_claim) that never reaches
//! [`record`](ProxyRecordingStore::record) (the upstream call failed) is released via
//! [`release_claim`](ProxyRecordingStore::release_claim), so the signature stays retryable
//! instead of wedging forever.
//!
//! This backend is **port-scoped**: [`RequestSignature`] carries no
//! port, so a manager-scoped store shared across imposters must key by port explicitly to keep
//! identical signatures on different ports from colliding.

use super::mode::ProxyMode;
use super::types::{RecordedResponse, RequestSignature};
use crate::extensions::decorate::BackendUnavailable;
use crate::imposter::Stub;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum number of recorded responses per signature (proxyAlways mode).
const MAX_RECORDINGS_PER_SIGNATURE: usize = 1000;

/// Maximum total number of unique request signatures to record (per port).
const MAX_TOTAL_SIGNATURES: usize = 10_000;

/// Opaque proof that the holder won the right to record a given `(port, signature)` once.
///
/// Issued by [`ProxyRecordingStore::try_claim`] and consumed by
/// [`record`](ProxyRecordingStore::record) / [`release_claim`](ProxyRecordingStore::release_claim).
/// A token is only valid until the claim it names is settled; a stale token (the claim expired
/// and was re-taken by another caller) is silently ignored so a late loser cannot clobber the
/// winner's recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimToken(u64);

impl ClaimToken {
    /// Construct a token from a raw value. Backends implementing [`ProxyRecordingStore`] mint
    /// their own tokens; the value only needs to be unique within the backend.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw token value.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Outcome of [`ProxyRecordingStore::try_claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller won the claim and must either [`record`](ProxyRecordingStore::record) a
    /// response or [`release_claim`](ProxyRecordingStore::release_claim) it.
    Claimed(ClaimToken),
    /// Another caller already holds the claim (proxyOnce, concurrent first requests). The
    /// caller may still proxy upstream but must not record.
    InFlight,
    /// A response is already recorded for this `(port, signature)`; the caller should replay
    /// via [`lookup`](ProxyRecordingStore::lookup).
    AlreadyRecorded,
}

/// Error returned by a [`ProxyRecordingStore`] that could not answer.
///
/// Two shapes, because a store failure means two genuinely different things and the engine must
/// do opposite things about them. The built-in [`LocalProxyStore`] returns neither.
///
/// `#[non_exhaustive]`: embedders `match` on this, and the two arms below are unlikely to be the
/// last word on how a distributed backend can fail — the same reasoning [`StubPlacement`] carries.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyStoreError {
    /// The backing store could not be reached, and the caller may **degrade**: forward upstream
    /// without recording. This is the right answer when the store is a persistence aid and the
    /// engine still enforces exactly-once itself — losing a recording costs a replay, not
    /// correctness.
    #[error("proxy recording store unavailable: {0}")]
    Unavailable(String),
    /// The store **is** the exactly-once arbiter and could neither grant nor deny the claim, so
    /// the caller must **not** forward: with nothing serializing claims, every request for the
    /// duration of the outage would reach the upstream, which is precisely what `proxyOnce`
    /// exists to prevent. The engine fails the request instead, and the response boundary answers
    /// it through [`backend_error_response`](crate::extensions::decorate::backend_error_response)
    /// — a `503` naming the carried `feature`/`detail`.
    ///
    /// Distinct from [`ClaimOutcome::InFlight`], which also forwards without recording: there a
    /// claim *was* serialized and the duplicate is bounded by the one racing window.
    #[error("proxy claim refused: {0}")]
    Refused(#[source] BackendUnavailable),
}

/// Convenience alias for fallible proxy-store operations.
pub type Result<T> = std::result::Result<T, ProxyStoreError>;

/// Where a proxy-recorded stub belongs relative to the proxy stub that produced it.
///
/// These are engine semantics, not a hint: a store that publishes stubs itself
/// ([`ProxyRecordingStore::publishes_stubs`]) has to reproduce them, and deriving them from the
/// proxy mode independently is how a publisher silently drifts from the engine.
///
/// `#[non_exhaustive]`: a downstream publisher `match`es on this, and a future placement must not
/// break every one of them — the same reasoning [`EventContext`](crate::imposter::EventContext)
/// carries. Match with a `_` arm and treat an unknown placement as unpublishable rather than
/// guessing a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StubPlacement {
    /// `proxyOnce`: insert **before** the proxy stub that answered, so the recording matches first
    /// next time.
    BeforeProxy,
    /// `proxyAlways`: merge the responses into the first stub **after** the proxy stub that
    /// answered whose predicates are structurally equal (issue #611; an empty list included), else
    /// **append at the end** of the list (issue #1389), as Mountebank does. The proxy keeps running
    /// and recording either way.
    AfterProxyMerging,
}

/// The stub the engine generated from a proxied response, together with where it belongs.
///
/// Handed to [`ProxyRecordingStore::complete`] before the stub is published, so a store can make
/// its own "recorded" commit conditional on publishing this successfully.
///
/// `#[non_exhaustive]`: embedders only ever read this type — the engine is the sole constructor —
/// so the context it will predictably grow must not become a breaking change to the seam whose
/// premise is stability. Same reasoning as [`EventContext`](crate::imposter::EventContext).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct StubPublication<'a> {
    /// The generated stub, exactly as the engine would insert it.
    pub stub: &'a Stub,
    /// Where the stub belongs relative to the anchor: the stub at
    /// [`anchor_index`](Self::anchor_index), else the first stub proxying to
    /// [`proxy_to`](Self::proxy_to).
    pub placement: StubPlacement,
    /// `proxy.to` of the proxy stub this recording came from. Locates the anchor when
    /// [`anchor_index`](Self::anchor_index) is `None`: the first stub proxying to it.
    pub proxy_to: &'a str,
    /// Index of the stub that answered the request when the recording was made — the anchor
    /// [`placement`](Self::placement) is relative to (issue #1389). `None` when that stub is no
    /// longer in the list, or when no stub answered. Read from the stub list before the stub is
    /// published, so a publisher applying recordings out of order may find it off by one.
    pub anchor_index: Option<usize>,
}

/// Pluggable proxy-recording backend, keyed by imposter port.
pub trait ProxyRecordingStore: Send + Sync {
    /// First caller per `(port, signature)` wins the right to record once. Returns
    /// [`ClaimOutcome::InFlight`] if another caller already holds the claim and
    /// [`ClaimOutcome::AlreadyRecorded`] if a response is already stored.
    ///
    /// `Err(`[`Unavailable`](ProxyStoreError::Unavailable)`)` = backend unavailable; the engine
    /// degrades and forwards without recording. `Err(`[`Refused`](ProxyStoreError::Refused)`)` =
    /// the store arbitrates exactly-once and could not answer; the engine fails the request and
    /// does **not** forward it. Built-ins never fail.
    fn try_claim(&self, port: u16, sig: &RequestSignature) -> Result<ClaimOutcome>;

    /// [`try_claim`](Self::try_claim) for a request answered by a proxy response in `mode` (issue
    /// #1387). Each proxy stub has its own `mode`, as in Mountebank: only a `proxyOnce` request is
    /// gated, and a `proxyAlways` one is always granted a claim, whatever other proxy stubs the
    /// imposter has. The engine calls this, never `try_claim`.
    ///
    /// The default defers to `try_claim`, for a store that resolves the mode itself. A store that
    /// wraps another must forward this too, or the wrapped store applies its constructor's mode.
    fn try_claim_as(
        &self,
        port: u16,
        sig: &RequestSignature,
        mode: ProxyMode,
    ) -> Result<ClaimOutcome> {
        let _ = mode;
        self.try_claim(port, sig)
    }

    /// Releases a claim the engine will not settle, so the signature stays retryable: after a
    /// failed forward or behavior, and when the request is dropped mid-flight (client disconnect,
    /// imposter stop, panic — issue #1193). The last case calls it from a destructor, possibly
    /// during runtime shutdown, so it must be synchronous and quick and must not assume an async
    /// context. Stale tokens (the claim expired and was re-taken) are ignored.
    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken);

    /// Records the proxied response against the claim named by `token`. A stale token is
    /// ignored (the recording is dropped) so a late loser cannot overwrite the winner.
    ///
    /// `Err` = backend unavailable (built-ins never fail). On `Err` the caller releases the
    /// claim (via [`release_claim`](Self::release_claim)), so an implementation that fails
    /// mid-record must leave the claim releasable — never hold `(port, signature)` pending
    /// after returning `Err`, or that signature wedges under `proxyOnce`.
    fn record(
        &self,
        port: u16,
        sig: RequestSignature,
        token: ClaimToken,
        resp: RecordedResponse,
    ) -> Result<()>;

    /// Settles the claim once the engine has built the stub the recording will be replayed from,
    /// and **before** that stub is published (issue #910).
    ///
    /// Called instead of [`record`](Self::record) whenever a stub was generated — that is, when
    /// `predicateGenerators`, `addWaitBehavior` or `addDecorateBehavior` is configured and
    /// predicate generation succeeded. When none of those is configured there is nothing to
    /// publish and the engine calls [`record`](Self::record) as before. When generation fails the
    /// engine takes no claim at all (issue #1333): recording a match-all stub would be wrong
    /// (issue #498), and replaying under a key without the request's identity would answer it with
    /// another request's recording.
    ///
    /// This ordering is the point: a store that publishes stubs to a shared or durable backend can
    /// make "this signature is Recorded" strictly conditional on its own publication ack, instead
    /// of committing it against a stub that does not exist yet.
    ///
    /// The predicates are generated from the request before the claim is taken, since with
    /// `predicateGenerators` they are part of the signature (issue #1333), so the claim is not
    /// held across a `predicateGenerators.inject` script.
    ///
    /// The default delegates to [`record`](Self::record), so a store that does not publish is
    /// unaffected.
    ///
    /// `Err` = backend unavailable, including "I could not publish the stub". As with
    /// [`record`](Self::record) the caller then releases the claim, so the signature stays
    /// retryable rather than wedging as Recorded-but-stub-less; the client still receives the
    /// upstream response, because the upstream call succeeded and only recording failed.
    fn complete(
        &self,
        port: u16,
        sig: RequestSignature,
        token: ClaimToken,
        resp: RecordedResponse,
        publication: &StubPublication<'_>,
    ) -> Result<()> {
        let _ = publication;
        self.record(port, sig, token, resp)
    }

    /// Whether this store publishes generated stubs itself.
    ///
    /// `true` makes the engine skip its own in-process stub insertion entirely, leaving the store
    /// as the only publisher — it must then honour the position carried by
    /// [`StubPublication::placement`]. `false` (the default) keeps the engine's insertion, and the
    /// stub handed to [`complete`](Self::complete) is informational.
    ///
    /// "Skip entirely" is unconditional, and that is the trade a publisher accepts: when no claim
    /// was won — a concurrent `proxyOnce` loser, or [`try_claim`](Self::try_claim) reporting the
    /// backend unavailable — there is no [`complete`](Self::complete) call *and* no local
    /// insertion, so that request's stub is published nowhere. The engine will not quietly keep a
    /// private copy the store's peers do not have.
    fn publishes_stubs(&self) -> bool {
        false
    }

    /// Returns the recorded response for replay, if any.
    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse>;

    /// Forgets the `proxyOnce` recording made under `sig` (issue #1386), so the next request with
    /// that signature is claimed, forwarded and recorded again. The engine calls this after a stub
    /// edit (delete, replace, reconcile) removes the stub that recording was inserted as: in
    /// Mountebank the stub *is* the recording, so a deleted recording is recorded anew.
    ///
    /// It is also called on the request path (issue #1705), when a request reaches a `proxyOnce`
    /// stub while every stub recorded under `sig` sits behind it: that recording did not answer,
    /// so the request is forwarded and recorded in front of the answering stub, as in Mountebank.
    /// It must therefore stay quick and synchronous. The engine claims `sig` again right after, so
    /// a store whose `forget_recorded` does nothing keeps replaying, as before.
    ///
    /// A claim still in flight is not a recording yet and must be left to settle. `Err` is logged
    /// by the engine, which does not undo the stub edit that has already happened. The default does
    /// nothing, so a store that cannot forget keeps replaying the recording, as before #1386.
    fn forget_recorded(&self, port: u16, sig: &RequestSignature) -> Result<()> {
        let _ = (port, sig);
        Ok(())
    }

    /// Clears all saved proxy responses for a port. Backs the DELETE endpoint for saved
    /// proxy responses and the manager's port reclaim on imposter deletion.
    fn clear(&self, port: u16);
}

/// Per-port recording state: the responses map plus the in-flight claim gate.
#[derive(Debug, Default)]
struct PortState {
    /// `proxyOnce` recordings: what the gate answers `AlreadyRecorded` from.
    responses: RwLock<HashMap<RequestSignature, Vec<RecordedResponse>>>,
    /// `proxyAlways` recordings, kept apart so a `proxyAlways` stub recording an identity never
    /// makes a `proxyOnce` stub replay it (issue #1387).
    always: RwLock<HashMap<RequestSignature, Vec<RecordedResponse>>>,
    /// Signatures with an in-flight `proxyOnce` claim, mapped to the active claim token.
    /// Presence is the TOCTOU-safe "someone is recording this" flag (#171/#118); the token
    /// lets [`record`](ProxyRecordingStore::record) and
    /// [`release_claim`](ProxyRecordingStore::release_claim) reject stale callers.
    pending: Mutex<HashMap<RequestSignature, u64>>,
}

/// Reference proxy-recording backend with the exact semantics of the embedded store it
/// replaced, plus the release-on-error fix and explicit port scoping.
///
/// The engine claims through [`try_claim_as`](ProxyRecordingStore::try_claim_as) with the mode of
/// the proxy response being served, so one store serves any mix of modes (issue #1387). The
/// constructor's mode is what [`try_claim`](ProxyRecordingStore::try_claim) applies, for a caller
/// that does not name one.
#[derive(Debug)]
pub struct LocalProxyStore {
    mode: ProxyMode,
    ports: RwLock<HashMap<u16, Arc<PortState>>>,
    /// Monotonic source of claim tokens, unique across ports and time.
    next_token: AtomicU64,
}

impl LocalProxyStore {
    /// Create a store whose [`try_claim`](ProxyRecordingStore::try_claim) applies `mode`.
    #[must_use]
    pub fn new(mode: ProxyMode) -> Self {
        Self {
            mode,
            ports: RwLock::new(HashMap::new()),
            next_token: AtomicU64::new(0),
        }
    }

    /// The mode [`try_claim`](ProxyRecordingStore::try_claim) applies.
    #[must_use]
    pub fn mode(&self) -> ProxyMode {
        self.mode
    }

    fn slot(&self, port: u16) -> Arc<PortState> {
        if let Some(slot) = self.ports.read().get(&port) {
            return Arc::clone(slot);
        }
        Arc::clone(self.ports.write().entry(port).or_default())
    }

    fn mint_token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::SeqCst)
    }

    #[cfg(test)]
    fn recorded_len(&self, port: u16, sig: &RequestSignature) -> usize {
        self.recordings(port, sig).len()
    }

    #[cfg(test)]
    fn recordings(&self, port: u16, sig: &RequestSignature) -> Vec<RecordedResponse> {
        let state = self.slot(port);
        let once = state.responses.read().get(sig).cloned().unwrap_or_default();
        let always = state.always.read().get(sig).cloned().unwrap_or_default();
        once.into_iter().chain(always).collect()
    }
}

/// Set on a token for a claim that does not gate (`proxyAlways`, `proxyTransparent`), so `record`
/// and `release_claim` tell the two kinds of claim apart without consulting a mode. Tokens are
/// counted from zero and never reach it.
const UNGATED: u64 = 1 << 63;

impl ProxyRecordingStore for LocalProxyStore {
    fn try_claim(&self, port: u16, sig: &RequestSignature) -> Result<ClaimOutcome> {
        self.try_claim_as(port, sig, self.mode)
    }

    fn try_claim_as(
        &self,
        port: u16,
        sig: &RequestSignature,
        mode: ProxyMode,
    ) -> Result<ClaimOutcome> {
        match mode {
            ProxyMode::ProxyOnce => {
                let state = self.slot(port);
                // Hold the responses read guard across the pending claim so record() (which
                // needs responses.write()) cannot land a new recording between the
                // "not recorded" check and the claim — the #171/#118 TOCTOU fix.
                let responses = state.responses.read();
                if responses.contains_key(sig) {
                    return Ok(ClaimOutcome::AlreadyRecorded);
                }
                let mut pending = state.pending.lock();
                if pending.contains_key(sig) {
                    return Ok(ClaimOutcome::InFlight);
                }
                let token = self.mint_token();
                pending.insert(sig.clone(), token);
                Ok(ClaimOutcome::Claimed(ClaimToken(token)))
            }
            // proxyAlways / proxyTransparent never gate: every request proxies. The claim is a
            // formality so the caller path is uniform.
            ProxyMode::ProxyAlways | ProxyMode::ProxyTransparent => Ok(ClaimOutcome::Claimed(
                ClaimToken(self.mint_token() | UNGATED),
            )),
        }
    }

    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
        if token.0 & UNGATED != 0 {
            return;
        }
        let state = self.slot(port);
        let mut pending = state.pending.lock();
        // Only release if the claim is still the one this token named; a stale token whose
        // claim was already re-taken must not free the new holder.
        if pending.get(sig).copied() == Some(token.0) {
            pending.remove(sig);
        }
    }

    fn record(
        &self,
        port: u16,
        sig: RequestSignature,
        token: ClaimToken,
        resp: RecordedResponse,
    ) -> Result<()> {
        let state = self.slot(port);
        if token.0 & UNGATED != 0 {
            // A store built for proxyTransparent records nothing an ungated claim brings.
            if self.mode == ProxyMode::ProxyTransparent {
                return Ok(());
            }
            let mut responses = state.always.write();
            if responses.len() >= MAX_TOTAL_SIGNATURES && !responses.contains_key(&sig) {
                tracing::debug!(
                    max = MAX_TOTAL_SIGNATURES,
                    "Recording store full, dropping new recording"
                );
                return Ok(());
            }
            let recordings = responses.entry(sig).or_default();
            if recordings.len() >= MAX_RECORDINGS_PER_SIGNATURE {
                tracing::debug!(
                    max = MAX_RECORDINGS_PER_SIGNATURE,
                    "Recording limit reached, dropping oldest"
                );
                recordings.remove(0);
            }
            recordings.push(resp);
            return Ok(());
        }
        // Lock order (responses then pending) matches try_claim to avoid deadlock.
        let mut responses = state.responses.write();
        let mut pending = state.pending.lock();
        // Stale or already-settled claim: drop the recording rather than clobber.
        if pending.get(&sig).copied() != Some(token.0) {
            tracing::debug!("Stale claim token; dropping recording (claim released/re-taken)");
            return Ok(());
        }
        if responses.len() >= MAX_TOTAL_SIGNATURES && !responses.contains_key(&sig) {
            tracing::debug!(
                max = MAX_TOTAL_SIGNATURES,
                "Recording store full, dropping new recording"
            );
            pending.remove(&sig);
            return Ok(());
        }
        responses.entry(sig.clone()).or_insert_with(|| vec![resp]);
        pending.remove(&sig);
        Ok(())
    }

    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
        let state = self.slot(port);
        let first = |map: &HashMap<RequestSignature, Vec<RecordedResponse>>| {
            map.get(sig)
                .and_then(|responses| responses.first().cloned())
        };
        // The engine looks up only after a proxyOnce `AlreadyRecorded`, so it must never be handed
        // a proxyAlways recording; a store built for proxyAlways still replays its own.
        if self.mode == ProxyMode::ProxyAlways {
            first(&state.always.read())
        } else {
            first(&state.responses.read())
        }
    }

    fn forget_recorded(&self, port: u16, sig: &RequestSignature) -> Result<()> {
        if let Some(state) = self.ports.read().get(&port) {
            state.responses.write().remove(sig);
        }
        Ok(())
    }

    fn clear(&self, port: u16) {
        if let Some(state) = self.ports.read().get(&port) {
            let mut once = state.responses.write();
            let mut always = state.always.write();
            once.clear();
            always.clear();
            state.pending.lock().clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unix_timestamp;

    fn sig(path: &str) -> RequestSignature {
        RequestSignature::new("GET", path, None, &[])
    }

    fn generated_stub() -> Stub {
        serde_json::from_value(serde_json::json!({
            "predicates": [{ "equals": { "path": "/test" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "recorded" } }],
        }))
        .expect("valid stub")
    }

    fn resp(status: u16, body: &str) -> RecordedResponse {
        RecordedResponse {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            latency_ms: Some(10),
            timestamp_secs: unix_timestamp(),
        }
    }

    fn claim_token(outcome: ClaimOutcome) -> ClaimToken {
        match outcome {
            ClaimOutcome::Claimed(t) => t,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    // AC6: proxyOnce records the first response only and replays it.
    #[test]
    fn local_proxy_once_records_first_only() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/test");

        let token = claim_token(store.try_claim(1, &s).unwrap());
        store
            .record(1, s.clone(), token, resp(200, "first"))
            .unwrap();

        // Now recorded: a second claim is refused, replay serves the first response.
        assert_eq!(
            store.try_claim(1, &s).unwrap(),
            ClaimOutcome::AlreadyRecorded
        );
        let recorded = store.lookup(1, &s).unwrap();
        assert_eq!(recorded.status, 200);
        assert_eq!(recorded.body, b"first");
        assert_eq!(store.recorded_len(1, &s), 1);
    }

    // AC6: proxyAlways appends every recording.
    #[test]
    fn local_proxy_always_records_all() {
        let store = LocalProxyStore::new(ProxyMode::ProxyAlways);
        let s = sig("/test");

        let t1 = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t1, resp(200, "first")).unwrap();
        let t2 = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t2, resp(201, "second")).unwrap();

        assert_eq!(store.recorded_len(1, &s), 2);
        // Replay serves the first.
        assert_eq!(store.lookup(1, &s).unwrap().status, 200);
    }

    // AC6: proxyAlways evicts the oldest recording past the per-signature cap.
    #[test]
    fn local_proxy_always_evicts_oldest() {
        let store = LocalProxyStore::new(ProxyMode::ProxyAlways);
        let s = sig("/evict");

        for i in 0..=MAX_RECORDINGS_PER_SIGNATURE {
            let t = claim_token(store.try_claim(1, &s).unwrap());
            store
                .record(1, s.clone(), t, resp(200, &format!("response-{i}")))
                .unwrap();
        }

        let recordings = store.recordings(1, &s);
        assert_eq!(recordings.len(), MAX_RECORDINGS_PER_SIGNATURE);
        assert_eq!(recordings[0].body, b"response-1", "oldest evicted first");
    }

    // AC6: proxyTransparent never records.
    #[test]
    fn local_proxy_transparent_never_records() {
        let store = LocalProxyStore::new(ProxyMode::ProxyTransparent);
        let s = sig("/test");

        let t = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t, resp(200, "body")).unwrap();

        assert!(store.lookup(1, &s).is_none());
        assert_eq!(store.recorded_len(1, &s), 0);
    }

    // Issue #1387: the gate is the requesting proxy's mode, not the mode the store was built with.
    #[test]
    fn a_proxy_always_claim_never_gates_on_a_store_built_for_proxy_once() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/always");

        let t1 = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap());
        store.record(1, s.clone(), t1, resp(200, "first")).unwrap();

        assert!(matches!(
            store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap(),
            ClaimOutcome::Claimed(_)
        ));
    }

    #[test]
    fn a_proxy_once_claim_gates_on_a_store_built_without_a_proxy() {
        let store = LocalProxyStore::new(ProxyMode::ProxyTransparent);
        let s = sig("/once");

        let t1 = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        assert_eq!(
            store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::InFlight
        );
        store.record(1, s.clone(), t1, resp(200, "first")).unwrap();

        assert_eq!(
            store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::AlreadyRecorded
        );
        assert_eq!(store.lookup(1, &s).unwrap().body, b"first");
    }

    // A proxyAlways recording of an identity is not a proxyOnce recording of it: a proxyOnce stub
    // reached with the same signature must still forward and record its own.
    #[test]
    fn a_proxy_always_recording_does_not_satisfy_a_proxy_once_claim() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/shared");

        let t1 = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap());
        store.record(1, s.clone(), t1, resp(200, "always")).unwrap();

        let t2 = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        store.record(1, s.clone(), t2, resp(201, "once")).unwrap();
        assert_eq!(
            store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::AlreadyRecorded
        );
        assert_eq!(store.lookup(1, &s).unwrap().body, b"once");
    }

    // Releasing a proxyAlways claim must not free a proxyOnce claim in flight for the signature.
    #[test]
    fn releasing_a_proxy_always_claim_leaves_a_proxy_once_claim_in_flight() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/inflight");

        let _once = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        let always = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap());
        store.release_claim(1, &s, always);

        assert_eq!(
            store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::InFlight
        );
    }

    // A proxyAlways claim still records every response, whatever mode the store was built with.
    #[test]
    fn a_proxy_always_claim_appends_on_a_store_built_for_proxy_once() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/append");

        for body in ["a", "b"] {
            let t = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap());
            store.record(1, s.clone(), t, resp(200, body)).unwrap();
        }

        assert_eq!(store.recorded_len(1, &s), 2);
    }

    // A stale proxyOnce token is dropped on any store, not appended as a proxyAlways recording.
    #[test]
    fn a_stale_proxy_once_token_is_dropped_on_a_store_built_for_proxy_always() {
        let store = LocalProxyStore::new(ProxyMode::ProxyAlways);
        let s = sig("/stale-always");

        let stale = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        store.release_claim(1, &s, stale);
        let _live = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        store
            .record(1, s.clone(), stale, resp(200, "late"))
            .unwrap();

        assert_eq!(store.recorded_len(1, &s), 0);
        assert!(store.lookup(1, &s).is_none());
    }

    // Releasing a proxyOnce claim frees it whatever mode the store was built with.
    #[test]
    fn a_released_proxy_once_claim_is_reclaimable_on_a_store_built_without_a_proxy() {
        let store = LocalProxyStore::new(ProxyMode::ProxyTransparent);
        let s = sig("/release");

        let t = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());
        store.release_claim(1, &s, t);

        assert!(matches!(
            store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::Claimed(_)
        ));
    }

    #[test]
    fn clear_drops_proxy_always_recordings_too() {
        let store = LocalProxyStore::new(ProxyMode::ProxyAlways);
        let s = sig("/clear");

        let t = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t, resp(200, "always")).unwrap();
        store.clear(1);

        assert_eq!(store.recorded_len(1, &s), 0);
        assert!(store.lookup(1, &s).is_none());
    }

    // `lookup` replays proxyAlways recordings only on a store built for proxyAlways: the engine
    // looks up after a proxyOnce `AlreadyRecorded` and must never get another stub's recording.
    #[test]
    fn lookup_serves_proxy_always_recordings_only_on_a_store_built_for_them() {
        let s = sig("/lookup");
        let always = LocalProxyStore::new(ProxyMode::ProxyAlways);
        let once = LocalProxyStore::new(ProxyMode::ProxyOnce);
        for store in [&always, &once] {
            let t = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyAlways).unwrap());
            store.record(1, s.clone(), t, resp(200, "always")).unwrap();
        }

        assert_eq!(always.lookup(1, &s).unwrap().body, b"always");
        assert!(once.lookup(1, &s).is_none());
    }

    // Issue #1386: forgetting a recording makes exactly that signature claimable again.
    #[test]
    fn forget_recorded_frees_exactly_that_signature() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let (a, b) = (sig("/a"), sig("/b"));
        for (port, s) in [(1, &a), (1, &b), (2, &a)] {
            let t = claim_token(store.try_claim_as(port, s, ProxyMode::ProxyOnce).unwrap());
            store.record(port, s.clone(), t, resp(200, "rec")).unwrap();
        }

        store.forget_recorded(1, &a).unwrap();

        assert!(matches!(
            store.try_claim_as(1, &a, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(store.lookup(1, &a).is_none());
        assert_eq!(
            store.try_claim_as(1, &b, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::AlreadyRecorded,
            "another signature on the port keeps its recording"
        );
        assert_eq!(
            store.try_claim_as(2, &a, ProxyMode::ProxyOnce).unwrap(),
            ClaimOutcome::AlreadyRecorded,
            "the same signature on another port keeps its recording"
        );
    }

    // A claim in flight is not a recording yet: forgetting leaves it pending, so it still settles.
    #[test]
    fn forget_recorded_leaves_an_in_flight_claim_alone() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/inflight");
        let t = claim_token(store.try_claim_as(1, &s, ProxyMode::ProxyOnce).unwrap());

        store.forget_recorded(1, &s).unwrap();
        store.record(1, s.clone(), t, resp(200, "settled")).unwrap();

        assert_eq!(store.lookup(1, &s).unwrap().body, b"settled");
    }

    // The trait default keeps a store that cannot forget replaying, as before #1386.
    #[test]
    fn the_default_forget_recorded_is_a_no_op() {
        struct Fixed;
        impl ProxyRecordingStore for Fixed {
            fn try_claim(&self, _: u16, _: &RequestSignature) -> Result<ClaimOutcome> {
                Ok(ClaimOutcome::AlreadyRecorded)
            }
            fn release_claim(&self, _: u16, _: &RequestSignature, _: ClaimToken) {}
            fn record(
                &self,
                _: u16,
                _: RequestSignature,
                _: ClaimToken,
                _: RecordedResponse,
            ) -> Result<()> {
                Ok(())
            }
            fn lookup(&self, _: u16, _: &RequestSignature) -> Option<RecordedResponse> {
                None
            }
            fn clear(&self, _: u16) {}
        }

        assert!(Fixed.forget_recorded(1, &sig("/x")).is_ok());
        assert_eq!(
            Fixed.try_claim(1, &sig("/x")).unwrap(),
            ClaimOutcome::AlreadyRecorded
        );
    }

    // The trait default defers to `try_claim`, so a store that resolves its own mode is unchanged.
    #[test]
    fn the_default_try_claim_as_defers_to_try_claim() {
        struct AlwaysRecorded;
        impl ProxyRecordingStore for AlwaysRecorded {
            fn try_claim(&self, _: u16, _: &RequestSignature) -> Result<ClaimOutcome> {
                Ok(ClaimOutcome::AlreadyRecorded)
            }
            fn release_claim(&self, _: u16, _: &RequestSignature, _: ClaimToken) {}
            fn record(
                &self,
                _: u16,
                _: RequestSignature,
                _: ClaimToken,
                _: RecordedResponse,
            ) -> Result<()> {
                Ok(())
            }
            fn lookup(&self, _: u16, _: &RequestSignature) -> Option<RecordedResponse> {
                None
            }
            fn clear(&self, _: u16) {}
        }

        assert_eq!(
            AlwaysRecorded
                .try_claim_as(1, &sig("/x"), ProxyMode::ProxyAlways)
                .unwrap(),
            ClaimOutcome::AlreadyRecorded
        );
    }

    // AC3: concurrent claimers get exactly one Claimed; the rest see InFlight.
    #[test]
    fn local_concurrent_try_claim_one_winner() {
        use std::thread;

        let store = Arc::new(LocalProxyStore::new(ProxyMode::ProxyOnce));
        let s = sig("/concurrent");
        let claimed = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                let s = s.clone();
                let claimed = Arc::clone(&claimed);
                thread::spawn(move || {
                    if let ClaimOutcome::Claimed(_) = store.try_claim(1, &s).unwrap() {
                        claimed.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            claimed.load(Ordering::SeqCst),
            1,
            "exactly one claimer wins in proxyOnce"
        );
    }

    // AC2 (the wedge regression): a released claim makes the signature reclaimable. Fails on
    // the pre-fix code where a failed upstream call left the signature pending forever.
    #[test]
    fn local_release_makes_signature_reclaimable() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/wedge");

        // First caller claims, then the upstream call "fails" so it releases.
        let t1 = claim_token(store.try_claim(1, &s).unwrap());
        store.release_claim(1, &s, t1);

        // A subsequent request must be able to claim and record — not be told InFlight forever.
        let outcome = store.try_claim(1, &s).unwrap();
        let t2 = claim_token(outcome);
        store
            .record(1, s.clone(), t2, resp(200, "recovered"))
            .unwrap();
        assert_eq!(store.lookup(1, &s).unwrap().body, b"recovered");
    }

    // AC4: a stale token (its claim expired and was re-taken) neither releases nor records.
    #[test]
    fn local_stale_token_release_and_record_ignored() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/stale");

        let t1 = claim_token(store.try_claim(1, &s).unwrap());
        store.release_claim(1, &s, t1); // claim freed
        let t2 = claim_token(store.try_claim(1, &s).unwrap()); // re-taken by a new caller

        // Stale release must not free t2's live claim.
        store.release_claim(1, &s, t1);
        assert_eq!(
            store.try_claim(1, &s).unwrap(),
            ClaimOutcome::InFlight,
            "stale release must not free the live claim"
        );

        // Stale record must be dropped, not stored.
        store.record(1, s.clone(), t1, resp(500, "stale")).unwrap();
        assert!(
            store.lookup(1, &s).is_none(),
            "stale record must be ignored"
        );

        // The live holder can still record.
        store.record(1, s.clone(), t2, resp(200, "fresh")).unwrap();
        assert_eq!(store.lookup(1, &s).unwrap().body, b"fresh");
    }

    // AC5: identical signatures on different ports do not collide through a shared store.
    #[test]
    fn local_ports_are_isolated() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/shared");

        let t = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t, resp(200, "port-1")).unwrap();

        // Port 2 sees the signature as fresh, not AlreadyRecorded.
        assert!(matches!(
            store.try_claim(2, &s).unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(store.lookup(2, &s).is_none());
        assert_eq!(store.lookup(1, &s).unwrap().body, b"port-1");
    }

    // clear() drops a port's recordings and reopens the gate.
    #[test]
    fn local_clear_resets_port() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/clear");

        let t = claim_token(store.try_claim(1, &s).unwrap());
        store.record(1, s.clone(), t, resp(200, "body")).unwrap();
        assert!(store.lookup(1, &s).is_some());

        store.clear(1);
        assert!(store.lookup(1, &s).is_none());
        assert!(matches!(
            store.try_claim(1, &s).unwrap(),
            ClaimOutcome::Claimed(_)
        ));
    }

    /// Implements only the methods that existed before #910 — the shape every pre-#910 store has.
    /// Its mere existence is the source-compatibility assertion; the tests below use it to prove
    /// the new defaults behave as documented.
    #[derive(Default)]
    struct LegacyStore {
        recorded: Mutex<Vec<(u16, RequestSignature, RecordedResponse)>>,
    }

    impl ProxyRecordingStore for LegacyStore {
        fn try_claim(&self, _port: u16, _sig: &RequestSignature) -> Result<ClaimOutcome> {
            Ok(ClaimOutcome::Claimed(ClaimToken::new(1)))
        }
        fn release_claim(&self, _port: u16, _sig: &RequestSignature, _token: ClaimToken) {}
        fn record(
            &self,
            port: u16,
            sig: RequestSignature,
            _token: ClaimToken,
            resp: RecordedResponse,
        ) -> Result<()> {
            self.recorded.lock().push((port, sig, resp));
            Ok(())
        }
        fn lookup(&self, _port: u16, _sig: &RequestSignature) -> Option<RecordedResponse> {
            None
        }
        fn clear(&self, _port: u16) {}
    }

    // AC2/AC5 (#910): a store written before `complete` existed still compiles, and the default
    // `complete` delegates to its `record`. Driven through `dyn` so object safety is asserted too.
    #[test]
    fn default_complete_delegates_to_record() {
        let store = LegacyStore::default();
        let stub = generated_stub();
        let publication = StubPublication {
            stub: &stub,
            placement: StubPlacement::BeforeProxy,
            proxy_to: "http://upstream",
            anchor_index: None,
        };

        let dynamic: &dyn ProxyRecordingStore = &store;
        assert!(
            !dynamic.publishes_stubs(),
            "a store publishes nothing unless it opts in"
        );
        dynamic
            .complete(
                7,
                sig("/legacy"),
                ClaimToken::new(1),
                resp(200, "ok"),
                &publication,
            )
            .expect("default complete delegates to record");

        let recorded = store.recorded.lock();
        assert_eq!(recorded.len(), 1, "the recording reached record()");
        assert_eq!(recorded[0].0, 7, "port is passed through");
        assert_eq!(recorded[0].2.body, b"ok");
    }

    // AC1/AC6 (#910): completing a claim through `complete` is recording — same visible result as
    // `record`, so `LocalProxyStore` is unchanged by the new seam.
    #[test]
    fn local_complete_records_like_record() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let s = sig("/complete");
        let stub = generated_stub();
        let publication = StubPublication {
            stub: &stub,
            placement: StubPlacement::BeforeProxy,
            proxy_to: "http://upstream",
            anchor_index: None,
        };

        let t = claim_token(store.try_claim(1, &s).unwrap());
        store
            .complete(1, s.clone(), t, resp(200, "done"), &publication)
            .unwrap();

        assert_eq!(store.lookup(1, &s).unwrap().body, b"done");
        assert_eq!(
            store.try_claim(1, &s).unwrap(),
            ClaimOutcome::AlreadyRecorded,
            "complete settles the claim exactly as record does"
        );
        assert!(!store.publishes_stubs());
    }

    // The total-signatures cap drops new signatures once full (proxyOnce), as before.
    #[test]
    fn local_total_signatures_cap_drops_new() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);

        for i in 0..MAX_TOTAL_SIGNATURES {
            let s = sig(&format!("/path-{i}"));
            let t = claim_token(store.try_claim(1, &s).unwrap());
            store.record(1, s, t, resp(200, "ok")).unwrap();
        }

        let overflow = sig("/overflow");
        let t = claim_token(store.try_claim(1, &overflow).unwrap());
        store
            .record(1, overflow.clone(), t, resp(200, "dropped"))
            .unwrap();
        assert!(
            store.lookup(1, &overflow).is_none(),
            "overflow signature dropped once the port is full"
        );
    }
}
