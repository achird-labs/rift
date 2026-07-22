//! Proxy server module.
//!
//! This module provides the proxy server implementation with support for:
//! - Fault injection (latency, error, TCP faults)
//! - Script-based fault decisions (Rhai, JavaScript)
//! - Mountebank-compatible response behaviors (wait, copy, lookup, decorate)
//! - Request recording and replay (proxyOnce, proxyAlways modes)
//! - Multi-upstream routing
//! - TLS/HTTPS support
//!
//! # Module Structure
//!
//! - `server` - ProxyServer struct and main run loop
//! - `handler` - Request handling and fault injection logic
//! - `forwarding` - Request forwarding to upstream servers
//! - `client` - HTTP client creation and configuration
//! - `tls` - TLS utilities and certificate handling
//! - `network` - Network listener utilities (SO_REUSEPORT)
//! - `response_ext` - Response extension traits for body transformations

mod client;
mod forwarding;
mod handler;
mod headers;
pub(crate) mod network;
mod response_ext;
mod server;
pub(crate) mod tls;

mod context;
pub mod intercept_ca;
#[cfg(test)]
mod tests;
pub mod truststore;

// Re-export public API types
// These are used by main.rs and may be used by external consumers
#[allow(unused_imports)]
pub use forwarding::error_response;
#[allow(unused_imports)]
pub use handler::rule_applies_to_upstream;
#[allow(unused_imports)]
pub use server::ProxyServer;
// TLS session-resumption config, shared with the intercept listener in rift-http-proxy (issue #705).
pub use tls::{TLS_SESSION_CACHE_SIZE, configure_session_resumption};
// HTTP connection-builder tuning, shared with the metrics/admin accept loops in rift-http-proxy
// (issue #716) — `network` itself stays `pub(crate)`, only this type is exposed.
pub use network::{DEFAULT_HTTP_MAX_BUF, HttpTuning};
// Accept-error handling shared by every listener in the workspace: the imposter serve loop
// (issue #750) and the admin API accept loop (issue #826), which must classify-and-retry rather
// than let one transient accept failure end the server.
pub use network::{
    AcceptBackoff, AcceptErrorClass, AcceptErrorEvent, AcceptErrorLog, classify_accept_error,
    is_fatal_listener_error,
};
