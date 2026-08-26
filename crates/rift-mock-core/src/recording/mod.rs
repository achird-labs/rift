//! Proxy recording for Mountebank-compatible record/replay functionality.
//!
//! Supports three modes:
//! - `proxyOnce`: Record first response, replay on subsequent matches
//! - `proxyAlways`: Always proxy, record all responses
//! - `proxyTransparent`: Always proxy, never record (default Rift behavior)
//!
//! `addWaitBehavior` and `predicateGenerators` are handled by the imposter `proxy` response
//! (`imposter::types::ProxyResponse`), not here; this module only stores what was recorded.
//! File-based persistence went with the reverse-proxy store in #975.
//!
//! # Module Structure
//!
//! - `mode` - Proxy recording mode enum
//! - `types` - Response and signature types
//! - `proxy_store` - the pluggable per-imposter recording backend

mod mode;
mod proxy_store;
mod types;

// Re-export main types
pub use mode::ProxyMode;
pub use proxy_store::{
    ClaimOutcome, ClaimToken, LocalProxyStore, ProxyRecordingStore, ProxyStoreError, StubPlacement,
    StubPublication,
};
pub use types::{RecordedResponse, RequestSignature};
