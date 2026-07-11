//! Pluggable proxy-recording backend (issue #315): the trait extracted from the concrete
//! per-imposter [`RecordingStore`](super::store::RecordingStore), so embedders can persist
//! recordings or share the `proxyOnce` exactly-once gate across engine instances.
//!
//! The default [`LocalProxyStore`] is behavior-identical to the embedded store it replaces —
//! same `proxyOnce`/`proxyAlways`/`proxyTransparent` semantics, the same TOCTOU fix
//! (#171/#118), and the same caps — **plus** a release-on-error fix: a claim taken by
//! [`try_claim`](ProxyRecordingStore::try_claim) that never reaches
//! [`record`](ProxyRecordingStore::record) (the upstream call failed) is released via
//! [`release_claim`](ProxyRecordingStore::release_claim), so the signature stays retryable
//! instead of wedging forever.
//!
//! Unlike the concrete store, this backend is **port-scoped**: [`RequestSignature`] carries no
//! port, so a manager-scoped store shared across imposters must key by port explicitly to keep
//! identical signatures on different ports from colliding.

use super::mode::ProxyMode;
use super::types::{RecordedResponse, RequestSignature};
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

/// Error returned by a [`ProxyRecordingStore`] whose backend is unavailable.
///
/// The built-in [`LocalProxyStore`] never returns this; it exists so external backends
/// (shared, persistent) can signal a transient failure and let the caller degrade gracefully.
#[derive(Debug, thiserror::Error)]
pub enum ProxyStoreError {
    /// The backing store could not be reached.
    #[error("proxy recording store unavailable: {0}")]
    Unavailable(String),
}

/// Convenience alias for fallible proxy-store operations.
pub type Result<T> = std::result::Result<T, ProxyStoreError>;

/// Pluggable proxy-recording backend, keyed by imposter port.
pub trait ProxyRecordingStore: Send + Sync {
    /// First caller per `(port, signature)` wins the right to record once. Returns
    /// [`ClaimOutcome::InFlight`] if another caller already holds the claim and
    /// [`ClaimOutcome::AlreadyRecorded`] if a response is already stored.
    ///
    /// `Err` = backend unavailable (built-ins never fail).
    fn try_claim(&self, port: u16, sig: &RequestSignature) -> Result<ClaimOutcome>;

    /// Releases a claim after a failed upstream call so the signature is retryable.
    /// Stale tokens (the claim expired and was re-taken) are ignored.
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

    /// Returns the recorded response for replay, if any.
    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse>;

    /// Clears all saved proxy responses for a port. Backs the DELETE endpoint for saved
    /// proxy responses and the manager's port reclaim on imposter deletion.
    fn clear(&self, port: u16);
}

/// Per-port recording state: the responses map plus the in-flight claim gate.
#[derive(Debug, Default)]
struct PortState {
    responses: RwLock<HashMap<RequestSignature, Vec<RecordedResponse>>>,
    /// Signatures with an in-flight `proxyOnce` claim, mapped to the active claim token.
    /// Presence is the TOCTOU-safe "someone is recording this" flag (#171/#118); the token
    /// lets [`record`](ProxyRecordingStore::record) and
    /// [`release_claim`](ProxyRecordingStore::release_claim) reject stale callers.
    pending: Mutex<HashMap<RequestSignature, u64>>,
}

/// Reference proxy-recording backend with the exact semantics of the embedded store it
/// replaced, plus the release-on-error fix and explicit port scoping.
///
/// A `LocalProxyStore` carries a single [`ProxyMode`]: the per-imposter default is constructed
/// with that imposter's mode. An embedder sharing one store across imposters via
/// [`with_proxy_store`](crate::imposter::ImposterManager::with_proxy_store) therefore shares one
/// mode across the ports it serves.
#[derive(Debug)]
pub struct LocalProxyStore {
    mode: ProxyMode,
    ports: RwLock<HashMap<u16, Arc<PortState>>>,
    /// Monotonic source of claim tokens, unique across ports and time.
    next_token: AtomicU64,
}

impl LocalProxyStore {
    /// Create a store for the given proxy mode.
    #[must_use]
    pub fn new(mode: ProxyMode) -> Self {
        Self {
            mode,
            ports: RwLock::new(HashMap::new()),
            next_token: AtomicU64::new(0),
        }
    }

    /// The proxy mode this store dispatches on.
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
        self.slot(port)
            .responses
            .read()
            .get(sig)
            .map_or(0, Vec::len)
    }

    #[cfg(test)]
    fn recordings(&self, port: u16, sig: &RequestSignature) -> Vec<RecordedResponse> {
        self.slot(port)
            .responses
            .read()
            .get(sig)
            .cloned()
            .unwrap_or_default()
    }
}

impl ProxyRecordingStore for LocalProxyStore {
    fn try_claim(&self, port: u16, sig: &RequestSignature) -> Result<ClaimOutcome> {
        match self.mode {
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
            // formality so the caller path is uniform; record() dispatches on mode.
            ProxyMode::ProxyAlways | ProxyMode::ProxyTransparent => {
                Ok(ClaimOutcome::Claimed(ClaimToken(self.mint_token())))
            }
        }
    }

    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
        if self.mode != ProxyMode::ProxyOnce {
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
        match self.mode {
            ProxyMode::ProxyOnce => {
                // Lock order (responses then pending) matches try_claim to avoid deadlock.
                let mut responses = state.responses.write();
                let mut pending = state.pending.lock();
                // Stale or already-settled claim: drop the recording rather than clobber.
                if pending.get(&sig).copied() != Some(token.0) {
                    tracing::debug!(
                        "Stale claim token; dropping recording (claim released/re-taken)"
                    );
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
            ProxyMode::ProxyAlways => {
                let mut responses = state.responses.write();
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
                Ok(())
            }
            // proxyTransparent never records.
            ProxyMode::ProxyTransparent => Ok(()),
        }
    }

    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
        self.slot(port)
            .responses
            .read()
            .get(sig)
            .and_then(|responses| responses.first().cloned())
    }

    fn clear(&self, port: u16) {
        if let Some(state) = self.ports.read().get(&port) {
            state.responses.write().clear();
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
