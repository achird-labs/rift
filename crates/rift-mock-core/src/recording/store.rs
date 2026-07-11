//! Recording store for proxy responses.

use super::mode::ProxyMode;
use super::stub_generator::generate_stub;
use super::types::{RecordedResponse, RequestSignature};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Maximum number of recorded responses per signature (proxyAlways mode)
const MAX_RECORDINGS_PER_SIGNATURE: usize = 1000;

/// Maximum total number of unique request signatures to record
const MAX_TOTAL_SIGNATURES: usize = 10_000;

/// Recording store for proxy responses
pub struct RecordingStore {
    /// Recorded responses by request signature
    responses: RwLock<HashMap<RequestSignature, Vec<RecordedResponse>>>,
    /// Signatures currently being proxied (in-flight), used to prevent
    /// TOCTOU races in proxyOnce mode where multiple concurrent requests
    /// could all see "not yet recorded" and proxy simultaneously.
    pending: Mutex<HashSet<RequestSignature>>,
    /// Mode-specific behavior
    mode: ProxyMode,
}

impl RecordingStore {
    pub fn new(mode: ProxyMode) -> Self {
        Self {
            responses: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            mode,
        }
    }

    /// Get the recording mode
    pub fn mode(&self) -> ProxyMode {
        self.mode
    }

    /// Record a response (for proxyOnce/proxyAlways modes)
    pub fn record(&self, signature: RequestSignature, response: RecordedResponse) {
        match self.mode {
            ProxyMode::ProxyOnce => {
                let mut store = self.responses.write();
                if store.len() >= MAX_TOTAL_SIGNATURES && !store.contains_key(&signature) {
                    debug!(
                        "Recording store full ({} signatures), dropping new recording",
                        MAX_TOTAL_SIGNATURES
                    );
                    self.pending.lock().remove(&signature);
                    return;
                }
                store
                    .entry(signature.clone())
                    .or_insert_with(|| vec![response]);
                // Remove from pending set now that the response is recorded
                self.pending.lock().remove(&signature);
            }
            ProxyMode::ProxyAlways => {
                let mut store = self.responses.write();
                if store.len() >= MAX_TOTAL_SIGNATURES && !store.contains_key(&signature) {
                    debug!(
                        "Recording store full ({} signatures), dropping new recording",
                        MAX_TOTAL_SIGNATURES
                    );
                    return;
                }
                let recordings = store.entry(signature).or_default();
                if recordings.len() >= MAX_RECORDINGS_PER_SIGNATURE {
                    debug!(
                        "Recording limit reached ({} per signature), dropping oldest",
                        MAX_RECORDINGS_PER_SIGNATURE
                    );
                    recordings.remove(0);
                }
                recordings.push(response);
            }
            ProxyMode::ProxyTransparent => {
                // Never record
            }
        }
    }

    /// Get recorded response for replay
    pub fn get_recorded(&self, signature: &RequestSignature) -> Option<RecordedResponse> {
        let store = self.responses.read();
        store
            .get(signature)
            .and_then(|responses| responses.first().cloned())
    }

    /// Atomically check whether to proxy and claim the signature if so.
    ///
    /// For `proxyOnce` mode, this prevents TOCTOU races: if multiple concurrent
    /// requests arrive for the same signature, only the first caller gets `true`.
    /// Subsequent callers see the signature as "pending" and get `false`.
    ///
    /// Returns `true` if the caller should proxy the request.
    /// Returns `false` if a recorded response exists or another request is already in-flight.
    pub fn should_proxy(&self, signature: &RequestSignature) -> bool {
        match self.mode {
            ProxyMode::ProxyOnce => {
                // Hold the read guard through the pending.insert() call so that
                // record() (which needs responses.write()) cannot complete between
                // the "not found" check and the pending claim, eliminating the TOCTOU.
                let responses = self.responses.read();
                if responses.contains_key(signature) {
                    return false;
                }
                // pending.lock() is acquired while `responses` read guard is still held.
                // record() cannot acquire responses.write() until we release the read
                // guard, so the check-and-claim is atomic with respect to record().
                self.pending.lock().insert(signature.clone())
                // `responses` guard dropped here
            }
            ProxyMode::ProxyAlways => true,
            ProxyMode::ProxyTransparent => true,
        }
    }

    /// Get all recorded responses (for export)
    // Public API for future use (mb replay export)
    pub fn get_all(&self) -> HashMap<RequestSignature, Vec<RecordedResponse>> {
        self.responses.read().clone()
    }

    /// Clear all recordings
    // Public API for future use (admin endpoints)
    pub fn clear(&self) {
        self.responses.write().clear();
        self.pending.lock().clear();
    }

    /// Get number of recorded signatures
    // Public API for future use (metrics/debugging)
    pub fn len(&self) -> usize {
        self.responses.read().len()
    }

    /// Check if empty
    // Public API for future use (metrics/debugging)
    pub fn is_empty(&self) -> bool {
        self.responses.read().is_empty()
    }

    /// Save recordings to file (JSON format)
    // Public API for persistence
    pub fn save_to_file(&self, path: &Path) -> Result<(), std::io::Error> {
        let data = self.responses.read();
        let serializable: Vec<_> = data
            .iter()
            .map(|(sig, responses)| (sig.clone(), responses.clone()))
            .collect();

        let json = serde_json::to_string_pretty(&serializable)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        fs::write(path, json)?;
        info!("Saved {} recordings to {:?}", data.len(), path);
        Ok(())
    }

    /// Load recordings from file (JSON format)
    // Public API for persistence
    pub fn load_from_file(&self, path: &Path) -> Result<usize, std::io::Error> {
        if !path.exists() {
            debug!("Recording file {:?} does not exist, starting fresh", path);
            return Ok(0);
        }

        let json = fs::read_to_string(path)?;
        let data: Vec<(RequestSignature, Vec<RecordedResponse>)> = serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let count = data.len();
        let mut store = self.responses.write();
        for (sig, responses) in data {
            store.insert(sig, responses);
        }

        info!("Loaded {} recordings from {:?}", count, path);
        Ok(count)
    }

    /// Export all recordings as Mountebank-compatible stubs
    // Public API for mb replay export
    pub fn export_as_stubs(
        &self,
        include_method: bool,
        include_path: bool,
        include_query: bool,
        include_headers: &[String],
    ) -> Vec<serde_json::Value> {
        let store = self.responses.read();
        store
            .iter()
            .flat_map(|(sig, responses)| {
                responses.iter().map(move |resp| {
                    generate_stub(
                        sig,
                        resp,
                        include_method,
                        include_path,
                        include_query,
                        include_headers,
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unix_timestamp;

    #[test]
    fn test_proxy_once_records_first_only() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        // First response should be recorded
        let resp1 = RecordedResponse {
            status: 200,
            headers: Vec::new(),
            body: b"first".to_vec(),
            latency_ms: Some(100),
            timestamp_secs: unix_timestamp(),
        };
        store.record(sig.clone(), resp1);

        // Second response should NOT be recorded (proxyOnce)
        let resp2 = RecordedResponse {
            status: 201,
            headers: Vec::new(),
            body: b"second".to_vec(),
            latency_ms: Some(50),
            timestamp_secs: unix_timestamp(),
        };
        store.record(sig.clone(), resp2);

        // Should return first response
        let recorded = store.get_recorded(&sig).unwrap();
        assert_eq!(recorded.status, 200);
        assert_eq!(recorded.body, b"first");
    }

    #[test]
    fn test_proxy_always_records_all() {
        let store = RecordingStore::new(ProxyMode::ProxyAlways);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"first".to_vec(),
                latency_ms: Some(100),
                timestamp_secs: unix_timestamp(),
            },
        );

        store.record(
            sig.clone(),
            RecordedResponse {
                status: 201,
                headers: Vec::new(),
                body: b"second".to_vec(),
                latency_ms: Some(50),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Should have 2 recordings
        let all = store.get_all();
        assert_eq!(all.get(&sig).unwrap().len(), 2);
    }

    #[test]
    fn test_proxy_transparent_never_records() {
        let store = RecordingStore::new(ProxyMode::ProxyTransparent);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"test".to_vec(),
                latency_ms: Some(100),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Should NOT be recorded
        assert!(store.get_recorded(&sig).is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn test_should_proxy() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        // Should proxy before recording
        assert!(store.should_proxy(&sig));

        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"test".to_vec(),
                latency_ms: Some(100),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Should NOT proxy after recording (replay instead)
        assert!(!store.should_proxy(&sig));
    }

    #[test]
    fn test_request_signature_with_query() {
        let sig1 = RequestSignature::new("GET", "/test", Some("a=1&b=2"), &[]);
        let sig2 = RequestSignature::new("GET", "/test", Some("a=1&b=2"), &[]);
        let sig3 = RequestSignature::new("GET", "/test", Some("a=1&b=3"), &[]);

        // Same signature should be equal
        assert_eq!(sig1, sig2);

        // Different query should be different
        assert_ne!(sig1, sig3);

        // Store should differentiate by query
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        store.record(
            sig1.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"response1".to_vec(),
                latency_ms: Some(10),
                timestamp_secs: unix_timestamp(),
            },
        );

        // sig2 should match sig1
        assert!(store.get_recorded(&sig2).is_some());

        // sig3 should not match
        assert!(store.get_recorded(&sig3).is_none());
    }

    #[test]
    fn test_request_signature_with_method() {
        let get_sig = RequestSignature::new("GET", "/test", None, &[]);
        let post_sig = RequestSignature::new("POST", "/test", None, &[]);

        // Different methods should produce different signatures
        assert_ne!(get_sig, post_sig);

        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        store.record(
            get_sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"GET response".to_vec(),
                latency_ms: Some(10),
                timestamp_secs: unix_timestamp(),
            },
        );

        // GET should have recording
        assert!(store.get_recorded(&get_sig).is_some());

        // POST should not have recording
        assert!(store.get_recorded(&post_sig).is_none());
    }

    #[test]
    fn test_proxy_always_should_always_proxy() {
        let store = RecordingStore::new(ProxyMode::ProxyAlways);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        // Should always proxy even after recording
        assert!(store.should_proxy(&sig));

        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"test".to_vec(),
                latency_ms: Some(100),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Still should proxy (proxyAlways always proxies)
        assert!(store.should_proxy(&sig));
    }

    #[test]
    fn test_proxy_transparent_should_always_proxy() {
        let store = RecordingStore::new(ProxyMode::ProxyTransparent);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        // Transparent mode always proxies
        assert!(store.should_proxy(&sig));
    }

    #[test]
    fn test_mode_accessor() {
        let once = RecordingStore::new(ProxyMode::ProxyOnce);
        assert_eq!(once.mode(), ProxyMode::ProxyOnce);

        let always = RecordingStore::new(ProxyMode::ProxyAlways);
        assert_eq!(always.mode(), ProxyMode::ProxyAlways);

        let transparent = RecordingStore::new(ProxyMode::ProxyTransparent);
        assert_eq!(transparent.mode(), ProxyMode::ProxyTransparent);
    }

    // =========================================================================
    // Issue #118: Race conditions — concurrent should_proxy in proxyOnce mode
    // =========================================================================

    #[test]
    fn test_proxy_once_concurrent_should_proxy_only_one_wins() {
        // In proxyOnce mode, only the first caller to should_proxy for a given
        // signature should get true. All subsequent callers should get false
        // until the response is recorded and the pending flag is cleared.
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(RecordingStore::new(ProxyMode::ProxyOnce));
        let sig = RequestSignature::new("GET", "/concurrent-test", None, &[]);

        let num_threads = 10;
        let wins = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let store = Arc::clone(&store);
                let sig = sig.clone();
                let wins = Arc::clone(&wins);
                thread::spawn(move || {
                    if store.should_proxy(&sig) {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            wins.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Only one thread should win should_proxy in proxyOnce mode"
        );
    }

    #[test]
    fn test_proxy_once_should_proxy_false_after_recording() {
        // After recording, both should_proxy and pending should be cleared
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        assert!(store.should_proxy(&sig), "First call should return true");
        assert!(
            !store.should_proxy(&sig),
            "Second call should return false (pending)"
        );

        // Record the response — clears pending
        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"ok".to_vec(),
                latency_ms: Some(10),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Now it should still return false (recorded)
        assert!(
            !store.should_proxy(&sig),
            "After recording, should return false"
        );
    }

    // =========================================================================
    // Issue #120: Recording store enforces size limits
    // =========================================================================

    #[test]
    fn test_proxy_always_evicts_oldest_when_limit_reached() {
        let store = RecordingStore::new(ProxyMode::ProxyAlways);
        let sig = RequestSignature::new("GET", "/evict-test", None, &[]);

        // Record MAX_RECORDINGS_PER_SIGNATURE + 1 responses
        for i in 0..=MAX_RECORDINGS_PER_SIGNATURE {
            store.record(
                sig.clone(),
                RecordedResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: format!("response-{i}").into_bytes(),
                    latency_ms: Some(10),
                    timestamp_secs: unix_timestamp(),
                },
            );
        }

        let all = store.get_all();
        let recordings = all.get(&sig).unwrap();
        assert_eq!(
            recordings.len(),
            MAX_RECORDINGS_PER_SIGNATURE,
            "Should not exceed MAX_RECORDINGS_PER_SIGNATURE"
        );

        // The oldest (response-0) should have been evicted
        assert_eq!(
            recordings[0].body, b"response-1",
            "Oldest recording should be evicted"
        );
    }

    #[test]
    fn test_recording_store_drops_new_signatures_when_full() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);

        // Fill up to MAX_TOTAL_SIGNATURES
        for i in 0..MAX_TOTAL_SIGNATURES {
            let sig = RequestSignature::new("GET", &format!("/path-{i}"), None, &[]);
            // Claim the signature first so record() succeeds
            store.should_proxy(&sig);
            store.record(
                sig,
                RecordedResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"ok".to_vec(),
                    latency_ms: Some(10),
                    timestamp_secs: unix_timestamp(),
                },
            );
        }

        assert_eq!(store.len(), MAX_TOTAL_SIGNATURES);

        // Adding a new signature should be silently dropped
        let new_sig = RequestSignature::new("GET", "/overflow", None, &[]);
        store.should_proxy(&new_sig);
        store.record(
            new_sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"dropped".to_vec(),
                latency_ms: Some(10),
                timestamp_secs: unix_timestamp(),
            },
        );

        assert_eq!(
            store.len(),
            MAX_TOTAL_SIGNATURES,
            "Store should not grow beyond MAX_TOTAL_SIGNATURES"
        );
        assert!(
            store.get_recorded(&new_sig).is_none(),
            "Overflow signature should not be recorded"
        );
    }

    // =========================================================================
    // Issue #171: TOCTOU fix — should_proxy must not race with record()
    // =========================================================================

    #[test]
    fn test_proxy_once_no_toctou_after_concurrent_record() {
        // Reproduce the TOCTOU: record() completes while should_proxy is between
        // its two operations (check responses, insert pending). With the fix,
        // should_proxy holds responses.read() across both operations, so record()
        // cannot complete mid-check.  We verify the observable invariant: after
        // record() has finished, should_proxy always returns false.
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(RecordingStore::new(ProxyMode::ProxyOnce));
        let sig = RequestSignature::new("GET", "/toctou-test", None, &[]);

        // Pre-record the response so the store already has it.
        store.should_proxy(&sig); // claim pending
        store.record(
            sig.clone(),
            RecordedResponse {
                status: 200,
                headers: Vec::new(),
                body: b"pre-recorded".to_vec(),
                latency_ms: Some(10),
                timestamp_secs: unix_timestamp(),
            },
        );

        // Spin up concurrent threads — all must see false because the response exists.
        let results: Vec<bool> = (0..20)
            .map(|_| {
                let store = Arc::clone(&store);
                let sig = sig.clone();
                thread::spawn(move || store.should_proxy(&sig))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();

        assert!(
            results.iter().all(|&r| !r),
            "All concurrent should_proxy calls after record() must return false"
        );
    }
}
