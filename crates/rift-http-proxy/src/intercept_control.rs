//! Shared runtime lifecycle for the intercept/TLS-MITM listener (issue #493).
//!
//! One process (or one FFI handle) owns at most a single intercept plane. Historically the three
//! surfaces that could start it — the `--intercept-port` CLI flag, the `rift_start_intercept` FFI
//! call, and (read-only) the `/intercept/*` admin routes — each held the listener differently and
//! could not be driven at runtime over the admin API. [`InterceptControl`] promotes the FFI's
//! `InterceptPlane` shape into a single cloneable slot that all three share, so start/stop/status
//! become one implementation and `POST`/`GET`/`DELETE /intercept` can manage the same listener a
//! CLI flag or FFI call started.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::intercept::InterceptListener;
use crate::intercept_rules::{InterceptRules, InterceptState};
use rift_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use serde::Serialize;

/// A running intercept plane: the listener plus the control-plane [`InterceptState`] (rule store +
/// CA) the admin routes and FFI mutate/export.
pub struct InterceptPlane {
    pub listener: InterceptListener,
    pub state: InterceptState,
}

/// Shared, mutable slot for the process's (or FFI handle's) single intercept plane. Cheap to clone
/// (an `Arc` inside). The `std` mutex is never held across an `.await` (see [`InterceptControl::start`]);
/// poisoning is recovered rather than propagated, like [`InterceptRules`].
#[derive(Clone, Default)]
pub struct InterceptControl(Arc<Mutex<Option<InterceptPlane>>>);

/// Start options — the exact shape (and serde attributes) of the FFI's former `InterceptOptions`,
/// so the admin `POST /intercept` body and `rift_start_intercept` parse identically.
/// `deny_unknown_fields` so a misspelled `caCertpath` is a hard error, not a silent fresh-CA
/// fallback that would defeat the caller's intended CA reuse.
#[derive(serde::Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterceptStartOptions {
    /// Bind host, default `127.0.0.1`.
    pub host: Option<String>,
    /// Bind port, default `0` (OS-assigned).
    pub port: Option<u16>,
    pub ca_cert_path: Option<String>,
    /// Both-or-neither with `ca_cert_path`.
    pub ca_key_path: Option<String>,
}

/// The running-listener status shared by `POST`/`GET /intercept` and `rift_start_intercept` — the
/// same field names and derivation so every surface returns a byte-compatible body.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterceptStatus {
    pub intercept_port: u16,
    pub intercept_url: String,
}

impl InterceptStatus {
    /// Derive both fields from the *real* bound address (OS-assigned port resolved). A `0.0.0.0`
    /// bind surfaces verbatim — dial a concrete interface.
    pub fn from_addr(addr: SocketAddr) -> Self {
        Self {
            intercept_port: addr.port(),
            intercept_url: format!("http://{addr}"),
        }
    }
}

/// Why an intercept `start` failed. The variants map 1:1 to the admin status codes (only
/// [`InterceptStartError::AlreadyRunning`] is a `409`; the rest are `400`) and to the FFI's
/// existing `rift_start_intercept: ...` `last_error` strings.
#[derive(Debug, thiserror::Error)]
pub enum InterceptStartError {
    #[error("intercept listener already running (one per process)")]
    AlreadyRunning,
    #[error("invalid host/port: {0}")]
    InvalidAddr(String),
    // `{0:#}` keeps the anyhow chain (missing file, bad PEM, mismatched pair). Never file contents.
    #[error("CA setup failed: {0:#}")]
    Ca(#[source] anyhow::Error),
    #[error("bind failed: {0}")]
    Bind(#[source] anyhow::Error),
}

impl InterceptControl {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<InterceptPlane>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True if a listener currently occupies the slot. A sync helper so the (`!Send`) `std`
    /// mutex guard never spans an `.await` in [`start`](Self::start).
    fn is_occupied(&self) -> bool {
        self.lock().is_some()
    }

    /// Install a freshly-bound plane, or hand the listener back if the slot filled in the meantime
    /// (a concurrent start won the race). Sync so the guard is confined to this frame.
    fn install(&self, plane: InterceptPlane) -> Result<(), InterceptListener> {
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(plane.listener);
        }
        *slot = Some(plane);
        Ok(())
    }

    /// Bind and install the listener. Fails with [`InterceptStartError::AlreadyRunning`] if the
    /// slot is occupied — including when it was occupied by the CLI flag or the FFI.
    ///
    /// The `std` mutex is never held across the async bind/shutdown: pre-check → bind (unlocked) →
    /// install-or-hand-back. Two concurrent starts therefore end with exactly one listener and one
    /// `AlreadyRunning`, and the loser's just-bound listener is shut down rather than leaked.
    pub async fn start(
        &self,
        opts: InterceptStartOptions,
    ) -> Result<SocketAddr, InterceptStartError> {
        if self.is_occupied() {
            return Err(InterceptStartError::AlreadyRunning);
        }

        let host = opts.host.as_deref().unwrap_or("127.0.0.1");
        let addr: SocketAddr = format!("{host}:{}", opts.port.unwrap_or(0))
            .parse()
            .map_err(|e| InterceptStartError::InvalidAddr(format!("{e}")))?;
        // Log CA/bind failures here so every surface (FFI, admin `POST /intercept`, CLI flag) gets a
        // server-side trail — not just the FFI, which used to `warn!` these on its own.
        let ca = Arc::new(
            CertificateAuthority::load_or_generate(
                opts.ca_cert_path.as_deref().map(Path::new),
                opts.ca_key_path.as_deref().map(Path::new),
            )
            .map_err(|e| {
                tracing::warn!(error = %e, "intercept start: CA setup failed");
                InterceptStartError::Ca(e)
            })?,
        );
        let rules = InterceptRules::new();
        let resolver = Arc::new(SniCertResolver::new(ca.clone()));
        let listener = InterceptListener::bind(addr, resolver, rules.clone())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "intercept start: bind failed");
                InterceptStartError::Bind(e)
            })?;
        let bound = listener.local_addr();

        match self.install(InterceptPlane {
            listener,
            state: InterceptState { rules, ca },
        }) {
            Ok(()) => Ok(bound),
            Err(listener) => {
                listener.shutdown().await;
                Err(InterceptStartError::AlreadyRunning)
            }
        }
    }

    /// Take the plane out of the slot and shut its listener down. Returns whether a listener was
    /// actually running (an idempotent no-op, returning `false`, otherwise).
    pub async fn stop(&self) -> bool {
        // Scope the guard so it is dropped before the `.await` below (it is `!Send`).
        let taken = { self.lock().take() };
        match taken {
            Some(plane) => {
                plane.listener.shutdown().await;
                true
            }
            None => false,
        }
    }

    /// Bound address of the running listener, if any.
    pub fn status(&self) -> Option<SocketAddr> {
        self.lock().as_ref().map(|p| p.listener.local_addr())
    }

    /// A clone of the running plane's [`InterceptState`] for the rules/CA/truststore handlers.
    /// Cheap: `InterceptState` is `Arc`-backed rules + an `Arc` CA.
    pub fn state(&self) -> Option<InterceptState> {
        self.lock().as_ref().map(|p| p.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> InterceptStartOptions {
        InterceptStartOptions::default()
    }

    #[tokio::test]
    async fn start_status_stop_roundtrip() {
        let control = InterceptControl::default();
        assert!(control.status().is_none());
        assert!(control.state().is_none());

        let addr = control.start(defaults()).await.expect("start");
        assert_eq!(control.status(), Some(addr));
        assert!(addr.port() > 0, "OS assigned a real port");
        assert!(control.state().is_some());

        assert!(control.stop().await, "stop reports it was running");
        assert!(control.status().is_none());
        assert!(control.state().is_none());
    }

    #[tokio::test]
    async fn double_start_is_already_running() {
        let control = InterceptControl::default();
        control.start(defaults()).await.expect("first start");
        let err = control.start(defaults()).await.expect_err("second start");
        assert!(matches!(err, InterceptStartError::AlreadyRunning));
        control.stop().await;
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let control = InterceptControl::default();
        assert!(!control.stop().await, "stop on empty slot is a no-op");
        control.start(defaults()).await.expect("start");
        assert!(control.stop().await);
        assert!(!control.stop().await, "second stop is a no-op");
    }

    #[tokio::test]
    async fn ca_paths_must_be_both_or_neither() {
        let control = InterceptControl::default();
        let opts = InterceptStartOptions {
            ca_cert_path: Some("only-cert.pem".to_string()),
            ..Default::default()
        };
        let err = control.start(opts).await.expect_err("half CA pair");
        assert!(matches!(err, InterceptStartError::Ca(_)));
        assert!(control.status().is_none(), "no listener left behind");
    }

    #[tokio::test]
    async fn bad_ca_path_is_ca_error() {
        let control = InterceptControl::default();
        let opts = InterceptStartOptions {
            ca_cert_path: Some("/no/such/cert.pem".to_string()),
            ca_key_path: Some("/no/such/key.pem".to_string()),
            ..Default::default()
        };
        let err = control.start(opts).await.expect_err("missing CA files");
        assert!(matches!(err, InterceptStartError::Ca(_)));
    }

    #[tokio::test]
    async fn occupied_port_is_bind_error() {
        // AC6: a port already in use surfaces as a `Bind` error (mapped to 400 at the handler).
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let port = occupied.local_addr().unwrap().port();
        let control = InterceptControl::default();
        let opts = InterceptStartOptions {
            port: Some(port),
            ..Default::default()
        };
        let err = control.start(opts).await.expect_err("port already bound");
        assert!(matches!(err, InterceptStartError::Bind(_)));
        assert!(control.status().is_none(), "no listener left behind");
    }

    #[tokio::test]
    async fn deny_unknown_fields() {
        let err = serde_json::from_str::<InterceptStartOptions>(r#"{"caCertpath":"x"}"#)
            .expect_err("misspelled field must be rejected");
        assert!(
            err.to_string().contains("caCertpath") || err.to_string().contains("unknown field")
        );
    }

    #[tokio::test]
    async fn concurrent_starts_bind_exactly_one_listener() {
        let control = InterceptControl::default();
        let (a, b) = tokio::join!(control.start(defaults()), control.start(defaults()));
        let winners = [&a, &b].iter().filter(|r| r.is_ok()).count();
        let losers = [&a, &b].iter().filter(|r| r.is_err()).count();
        assert_eq!(winners, 1, "exactly one start wins");
        assert_eq!(losers, 1, "exactly one start loses");
        // The winner's port is the one installed; the loser's listener was shut down, not leaked.
        let installed = control.status().expect("a listener is installed");
        let won = a.or(b).expect("the Ok addr");
        assert_eq!(installed, won, "the installed listener is the winner's");
        control.stop().await;
    }
}
