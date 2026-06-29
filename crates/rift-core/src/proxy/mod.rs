//! Proxy server module.
//!
//! This module provides the proxy server implementation with support for:
//! - Fault injection (latency, error, TCP faults)
//! - Script-based fault decisions (Rhai, Lua, JavaScript)
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
mod network;
mod response_ext;
mod server;
mod tls;

mod context;
#[cfg(test)]
mod tests;

// Re-export public API types
// These are used by main.rs and may be used by external consumers
#[allow(unused_imports)]
pub use forwarding::error_response;
#[allow(unused_imports)]
pub use handler::rule_applies_to_upstream;
#[allow(unused_imports)]
pub use server::ProxyServer;
