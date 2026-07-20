//! Rift core engine — CLI-free imposter lifecycle, matching, behaviors, scripting and
//! flow-state. Usable in-process without the admin HTTP server or the `clap` CLI (issue #203);
//! the `rift-http-proxy` server and the `rift-ffi` C-ABI are thin consumers.
#![allow(dead_code)]

/// Whether the quamina-backed body-field candidate dimension is compiled into this build.
///
/// Reported at startup by the server binary so a benchmark — or an operator — can read the answer
/// off the artefact instead of inferring it from build flags, the same way `Global allocator:`
/// (#717) and `Runtime topology:` (RFC-712) already are. It is deliberately defined *here*, in the
/// crate whose `#[cfg]` gates the dimension, rather than in a consumer: issue #777 shipped this
/// dimension enabled in `rift-mock-core` and compiled out of both the binary and the C-ABI,
/// because each consumer takes this crate with `default-features = false` and nothing forwarded
/// the feature. A marker sourced from a consumer would have reported that consumer's own opinion;
/// this one reports what actually got compiled.
pub const QUAMINA_BODY_FIELD_DIMENSION: bool = cfg!(feature = "quamina-matching");

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
pub use extensions::stub_analysis;
pub use extensions::template;

// Shared utilities
pub mod util;
// Embedded-Rust consumers name the aliases as `rift_mock_core::FastMap` (issue #704).
pub use util::{FastMap, FastSet};

// Backends (pub for integration tests)
pub mod backends;

// Scripting validation/execution (pub so the admin server can validate stubs)
pub mod scripting;
