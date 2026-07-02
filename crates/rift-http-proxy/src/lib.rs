// Library exports for benchmarking and testing
// Allow dead_code for library targets - functions are used by the binary but not by tests
#![allow(dead_code)]

// The CLI-free engine now lives in the `rift-core` crate (issue #203). Re-export its modules at
// the crate root so existing `crate::<module>` paths in the admin server, CLI and tests keep
// resolving unchanged — the server is a thin consumer of the core.
pub use rift_core::{
    backends, behaviors, config, extensions, fault, flow_state, imposter, matcher, predicate,
    proxy, recording, response, routing, rule_index, scripting, stub_analysis, template, util,
};

// ===== Admin HTTP server (control plane — server crate only) =====
pub mod admin_api;

// Imposter config loading (--configfile / --datadir), shared with hot-reload (issue #197)
pub mod config_loader;

// ===== Embeddable server composition (issue #317) =====
// Gateway dispatch (issue #212) callable from any listener
pub mod gateway;
// CLI surface + ServerBuilder + metrics server; the `rift` binary is a thin caller
pub mod server;
