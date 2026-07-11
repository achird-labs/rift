//! Rift core engine — CLI-free imposter lifecycle, matching, behaviors, scripting and
//! flow-state. Usable in-process without the admin HTTP server or the `clap` CLI (issue #203);
//! the `rift-http-proxy` server and the `rift-ffi` C-ABI are thin consumers.
#![allow(dead_code)]

// ===== Core Mountebank-compatible modules =====
pub mod behaviors;
pub mod config;
pub mod imposter;
pub mod predicate;
pub mod proxy;
pub mod recording;

// ===== Rift Extensions (features beyond Mountebank) =====
pub mod extensions;
pub mod response;

// Re-export extension modules at top level for backward compatibility
pub use extensions::fault;
pub use extensions::flow_state;
pub use extensions::matcher;
pub use extensions::routing;
pub use extensions::rule_index;
pub use extensions::stub_analysis;
pub use extensions::template;

// Shared utilities
pub mod util;

// Backends (pub for integration tests)
pub mod backends;

// Scripting validation/execution (pub so the admin server can validate stubs)
pub mod scripting;
