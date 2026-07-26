// Library exports for benchmarking and testing
// Allow dead_code for library targets - functions are used by the binary but not by tests
#![allow(dead_code)]

// The CLI-free engine now lives in the `rift-mock-core` crate (issue #203). Re-export its modules at
// the crate root so existing `crate::<module>` paths in the admin server, CLI and tests keep
// resolving unchanged — the server is a thin consumer of the core.
pub use rift_mock_core::{
    backends, behaviors, config, extensions, fault, flow_state, imposter, matcher, predicate,
    proxy, recording, response, routing, scripting, stub_analysis, template, util,
};

/// The named flow-state backends this build ships (issue #853).
///
/// One owner for "which backends exist in a shipped artifact", so the binary and the C-ABI cannot
/// drift apart: both register the result of this on their `ImposterManager`. `"redis"` is present
/// exactly when the default `redis-backend` feature is on — that feature used to enable the store
/// inside `rift-mock-core`, and now pulls in the `rift-store-redis` crate instead, which is why
/// the extraction is invisible to users.
///
/// A backend absent here is not a silent downgrade: naming it in `_rift.flowState.backend` fails
/// imposter creation with an error listing what is available (issues #325/#377).
#[must_use]
pub fn default_flow_store_backends() -> extensions::flow_state::FlowStoreBackends {
    let backends = extensions::flow_state::FlowStoreBackends::new();
    #[cfg(feature = "redis-backend")]
    let backends = backends.with(std::sync::Arc::new(
        rift_store_redis::RedisFlowStoreBackendFactory,
    ));
    backends
}

/// Install the process-wide rustls `ring` crypto provider, idempotently (issue #343).
///
/// The binary does this in `main.rs`; an embedded host (the FFI `rift_start`) must too, or an
/// HTTPS imposter hits the missing-provider path. Safe to call more than once — a provider is
/// already-installed error is ignored, so this composes with a host that installed its own.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// The `--allowInjection` classifier, shared by every door that admits an imposter config
// (admin API, --configfile, --datadir, POST /admin/reload) so they cannot diverge (issue #612)
pub mod injection_gate;

// ===== Admin HTTP server (control plane — server crate only) =====
pub mod admin_api;

// Inbound forward-proxy intercept listener (TLS-MITM, epic #394 slice 3)
pub mod intercept;

// Intercept rules (predicate match -> serve/forward) + admin control state (epic #394 slice 4)
pub mod intercept_rules;

// Shared runtime lifecycle (start/stop/status) for the intercept listener, driven by the CLI
// flag, the admin `/intercept` routes, and the FFI over one cloneable slot (issue #493)
pub mod intercept_control;

// Imposter config loading (--configfile / --datadir), shared with hot-reload (issue #197)
pub mod config_loader;

/// Imposter sources (U-12): `--imposters <uri,...>` and the `ImposterSource` SPI embedders
/// register their own schemes through. `file:`/`https:` are built in; parsing is shared with
/// [`config_loader`] so no scheme can grow its own dialect.
pub mod sources;

// `rift script check` / `rift script run` (issue #360): scripting DX outside a running server
pub mod script_cli;

// ===== Embeddable server composition (issue #317) =====
// Gateway dispatch (issue #212) callable from any listener
pub mod gateway;

/// The front door: one listener routing to many imposters by host/path/header
/// (issue #19). The gateway above addresses imposters by port; this addresses
/// them by what the request says.
pub mod front_door;
// CLI surface + ServerBuilder + metrics server; the `rift` binary is a thin caller
pub mod server;

// rcfile/stop/save bootstrap helpers shared with alternative binaries (issue #807)
pub mod bootstrap;

/// Opt-in per-core runtime topology for the server binary (RFC-712, issue #744).
pub mod runtime;

/// `rift healthcheck` (issue #664): the container HEALTHCHECK probe, built into the binary so the
/// image needs no shell or curl.
pub mod healthcheck;
