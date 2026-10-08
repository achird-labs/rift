// The CLI-free engine now lives in the `rift-mock-core` crate (issue #203). Re-export its modules at
// the crate root so existing `crate::<module>` paths in the admin server, CLI and tests keep
// resolving unchanged — the server is a thin consumer of the core.
//
// The TCP-fault names are the non-module names in this block: they are re-exported by value so an
// embedder holding only `rift-http-proxy` names them at the root exactly as it would on
// `rift-mock-core` (issues #965, #1234), not via `imposter::fault_io`.
pub use rift_mock_core::{
    FaultCell, FaultIo, InjectedFault, TcpFaultKind, backends, behaviors, config, extensions,
    flow_state, imposter, is_injected_fault, proxy, recording, response, scripting, stub_analysis,
    tcp_fault_carrier, template, util,
};

/// The named flow-state backends this build ships (issue #853).
///
/// One owner for "which backends exist in a shipped artifact", so the binary and the C-ABI cannot
/// drift apart: both register the result of this on their `ImposterManager`. The registry is empty
/// because `"inmemory"` is built in; the function stays as the single seam a future named backend
/// would register through.
///
/// A backend absent here is not a silent downgrade: naming it in `_rift.flowState.backend` fails
/// imposter creation with an error listing what is available (issues #325/#377).
#[must_use]
pub fn default_flow_store_backends() -> extensions::flow_state::FlowStoreBackends {
    extensions::flow_state::FlowStoreBackends::new()
}

/// Install the process-wide rustls `ring` crypto provider, idempotently (issue #343).
///
/// The binary does this in `main.rs`; an embedded host (the FFI `rift_start`) must too, or an
/// HTTPS imposter hits the missing-provider path. Safe to call more than once — a provider is
/// already-installed error is ignored, so this composes with a host that installed its own.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A reqwest builder for Rift's own command-line clients (`rift healthcheck`, `rift save`,
/// `rift-verify`): the `ring` provider and the OS trust store, verified by webpki.
///
/// That is what reqwest 0.12's `rustls-tls-native-roots` gave every client, and what these
/// clients keep under 0.13, whose own default would be `rustls-platform-verifier`. Two
/// differences matter here: the platform verifier refuses to build at all on a host with an empty
/// trust store (so `rift healthcheck` against `http://localhost` would fail in a CA-less image),
/// and on macOS/Windows it defers to the OS policy engine instead of webpki.
///
/// reqwest 0.13 also reads the crypto provider from the process default rather than linking one
/// itself, so this installs `ring` first (idempotent; a host's own choice wins). The trust anchors
/// are read once per process. `add_root_certificate` and `danger_accept_invalid_certs` compose
/// with the result as before. Imposter traffic does not use this: proxy stubs, config sources and
/// intercept forwarding build their clients from [`proxy::OutboundTls`].
pub fn http_client_builder() -> reqwest::ClientBuilder {
    install_default_crypto_provider();
    reqwest::Client::builder().tls_certs_only(native_root_certificates().iter().cloned())
}

/// The OS trust store as reqwest certificates, loaded once. Unreadable entries are skipped with a
/// warning, as reqwest 0.12 skipped them; an empty store is not an error here (see
/// [`http_client_builder`]), it just means no `https://` origin verifies.
fn native_root_certificates() -> &'static [reqwest::Certificate] {
    static ROOTS: std::sync::OnceLock<Vec<reqwest::Certificate>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| {
        let native = rustls_native_certs::load_native_certs();
        for error in &native.errors {
            tracing::warn!(%error, "ignoring an unreadable entry in the OS trust store");
        }
        native
            .certs
            .iter()
            .filter_map(|der| match reqwest::Certificate::from_der(der) {
                Ok(cert) => Some(cert),
                Err(error) => {
                    tracing::warn!(%error, "ignoring an OS trust anchor reqwest cannot use");
                    None
                }
            })
            .collect()
    })
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

/// `rift intercept-ca` (issue #1274): make a persistent intercept CA and its truststores offline.
pub mod intercept_ca_cli;
