//! Proxy recording for Mountebank-compatible record/replay functionality.
//!
//! Supports three modes:
//! - `proxyOnce`: Record first response, replay on subsequent matches
//! - `proxyAlways`: Always proxy, record all responses
//! - `proxyTransparent`: Always proxy, never record (default Rift behavior)
//!
//! Features:
//! - `addWaitBehavior`: Capture actual latency in recorded responses
//! - `predicateGenerators`: Auto-generate stubs from recorded requests
//! - File-based persistence for recordings
//!
//! # Module Structure
//!
//! - `mode` - Proxy recording mode enum
//! - `types` - Response and signature types
//! - `store` - Recording store implementation
//! - `stub_generator` - Mountebank stub generation

mod mode;
mod store;
mod stub_generator;
mod types;

// Re-export main types
pub use mode::ProxyMode;
pub use store::RecordingStore;
#[allow(unused_imports)]
pub use stub_generator::generate_stub;
pub use types::{RecordedResponse, RequestSignature};

use std::time::Instant;

/// Record a response with timing
// Public API for future use (higher-level recording helper)
pub fn record_with_timing<F, T>(store: &RecordingStore, signature: RequestSignature, f: F) -> T
where
    F: FnOnce() -> (T, u16, Vec<(String, String)>, Vec<u8>),
{
    let start = Instant::now();
    let (result, status, headers, body) = f();
    let latency_ms = start.elapsed().as_millis() as u64;

    let response = RecordedResponse {
        status,
        headers,
        body,
        latency_ms: Some(latency_ms),
        timestamp_secs: crate::util::unix_timestamp(),
    };

    store.record(signature, response);
    result
}
