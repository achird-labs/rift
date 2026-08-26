//! Shared proxy/TLS utilities.
//!
//! This module used to host the reverse-proxy server and its request pipeline.
//! That mode was unwired from the binary in ada6f30 (2025-11-30) and removed in #975; what is left
//! is the TLS and listener machinery the imposter path and the intercept listener share.
//!
//! # Module Structure
//!
//! - `tls` - TLS acceptor construction and session resumption
//! - `outbound_tls` - the shared trust policy for connections Rift initiates (#974)
//! - `intercept_ca` - the TLS-MITM certificate authority and per-SNI resolver
//! - `truststore` - PKCS#12 / JKS export of that CA
//! - `network` - listener utilities (SO_REUSEPORT) and accept-error classification

pub(crate) mod network;
pub(crate) mod tls;

pub mod intercept_ca;
pub mod outbound_tls;
pub mod truststore;

// Re-export public API types
// These are used by main.rs and may be used by external consumers
// TLS session-resumption config, shared with the intercept listener in rift-http-proxy (issue #705).
pub use tls::{TLS_SESSION_CACHE_SIZE, configure_session_resumption};
// One outbound-TLS trust policy for every client Rift initiates a connection with (issue #974).
pub use outbound_tls::OutboundTls;
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
