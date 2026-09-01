//! Admin REST API for Rift proxy and imposter management.
//!
//! This module provides a Mountebank-compatible REST API for:
//! - Creating, deleting, and listing imposters
//! - Managing stubs within imposters
//! - Clearing recorded requests and proxy responses
//! - Health and metrics endpoints
//!
//! The API listens on a configurable port (default: 2525).

// Public because the `AdminAuthorizer` hook (#854) is only usable when upstream parses your
// routes. An embedder that terminates some admin routes itself needs the same classifier, and the
// alternative — a second route parser — is the divergence `authz`'s own doc comment records as a
// shipped security bug (issue #887).
pub mod authz;
mod handlers;
mod request_filter;
mod router;
mod server;
pub mod types;

pub use handlers::imposters::{filter_proxy_responses, filter_proxy_stubs};
/// The space-stub shape guard (issue #336), for an embedder that terminates
/// `POST /imposters/:port/spaces/:flowId/stubs` itself rather than proxying to the handler here.
pub use handlers::scenarios::not_a_stub_reason;
pub use server::{
    AdminApiServer, AdminExposurePolicy, RunningAdminApi, check_admin_exposure,
    check_intercept_exposure, validate_admin_api_key,
};

/// Default port for the Mountebank-compatible admin API.
pub const DEFAULT_ADMIN_PORT: u16 = 2525;

/// Every key the embedded serve-options document accepts (`rift_serve_admin`'s JSON, issue #877),
/// published verbatim by `rift_build_info().serveOptions` and `GET /config`.
///
/// This exists because `rift_abi_version`'s "additive symbols are discovered by presence"
/// convention cannot reach a JSON *field* — there is no symbol for an SDK to probe. Publishing the
/// accepted keys is the only mechanism that works against **already-released** engines: an older
/// engine simply has no `serveOptions` key, so its absence is the signal. A `deny_unknown_fields`
/// rejection cannot do that, because the engine doing the ignoring is the old one.
///
/// It lives here rather than beside `ServeOptions` in `rift-ffi` so the C-ABI and the admin API can
/// publish one list instead of two that drift; `rift-ffi` already depends on this crate.
/// `rift-ffi`'s `serve_option_keys_match_the_struct_exactly` pins it to the struct.
pub const SERVE_OPTION_KEYS: &[&str] = &[
    "host",
    "port",
    "apiKey",
    "metricsPort",
    "configFile",
    "config",
    "allowInjection",
    "requireAdminAuth",
    "upstreamCaFile",
    "upstreamCaPem",
    "upstreamTlsSkipVerify",
];
