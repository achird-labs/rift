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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::intercept::InterceptListener;
use crate::intercept_rules::{InterceptRule, InterceptRules, InterceptState, RulesAtCapacity};
use rift_mock_core::proxy::intercept_ca::{CaSource, CertificateAuthority, SniCertResolver};
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
/// fallback that would defeat the caller's intended CA reuse. A pre-#593 engine's
/// `deny_unknown_fields` also gives SDKs deterministic feature detection: an unknown `caCertPem`
/// or `returnCaKey` is a hard 400 naming the field, not a silent ignore.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterceptStartOptions {
    /// Bind host, default `127.0.0.1`.
    pub host: Option<String>,
    /// Bind port, default `0` (OS-assigned).
    pub port: Option<u16>,
    pub ca_cert_path: Option<String>,
    /// Both-or-neither with `ca_cert_path`.
    pub ca_key_path: Option<String>,
    /// Inline CA certificate PEM (issue #593) — both-or-neither with `ca_key_pem`, mutually
    /// exclusive with the path pair. Lets a containerized engine be handed a CA over the admin
    /// API without a filesystem mount.
    pub ca_cert_pem: Option<String>,
    /// Inline CA private-key PEM (issue #593). Secret material — never logged (see the `Debug` impl).
    pub ca_key_pem: Option<String>,
    /// Generate a fresh CA and return its cert **and** key in the start response (issue #593).
    /// Only valid when no CA source is supplied — combining it with a path/PEM pair is a `400`.
    pub return_ca_key: Option<bool>,
    /// Rules to install before the listener accepts anything (issue #655). Lets one declarative
    /// document — a `--configfile` `intercept` block, a `POST /intercept` body, or an FFI start —
    /// bring up a listener that is already correct, instead of requiring a follow-up
    /// `POST /intercept/rules` that traffic can race.
    #[serde(default)]
    pub rules: Vec<InterceptRule>,
}

impl std::fmt::Debug for InterceptStartOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the inline key PEM — it is secret material (issue #593). Paths and the cert are safe.
        f.debug_struct("InterceptStartOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("ca_cert_path", &self.ca_cert_path)
            .field("ca_key_path", &self.ca_key_path)
            .field("ca_cert_pem", &self.ca_cert_pem.as_ref().map(|_| "<pem>"))
            .field(
                "ca_key_pem",
                &self.ca_key_pem.as_ref().map(|_| "<redacted>"),
            )
            .field("return_ca_key", &self.return_ca_key)
            // Count only: a rule's serve body can be an arbitrarily large payload.
            .field("rules", &self.rules.len())
            .finish()
    }
}

/// Outcome of a successful [`InterceptControl::start`]: the bound address plus, when the caller set
/// `return_ca_key` on a generated CA, the CA's `(cert_pem, key_pem)` to hand back exactly once.
pub struct StartedIntercept {
    pub(crate) addr: SocketAddr,
    pub(crate) ca_export: Option<(String, String)>,
}

impl std::fmt::Debug for StartedIntercept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show only whether a CA was exported, never the key material (issue #593).
        f.debug_struct("StartedIntercept")
            .field("addr", &self.addr)
            .field(
                "ca_export",
                &self.ca_export.as_ref().map(|_| "<ca cert+key>"),
            )
            .finish()
    }
}

/// The running-listener status shared by `POST`/`GET /intercept` and `rift_start_intercept` — the
/// same field names and derivation so every surface returns a byte-compatible body. The `caCertPem`
/// /`caKeyPem` fields are populated **only** by `POST /intercept` with `returnCaKey` (issue #593);
/// `GET /intercept` never carries key material.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterceptStatus {
    pub intercept_port: u16,
    pub intercept_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert_pem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_key_pem: Option<String>,
}

impl InterceptStatus {
    /// Derive the address fields from the *real* bound address (OS-assigned port resolved), with no
    /// CA export. A `0.0.0.0` bind surfaces verbatim — dial a concrete interface.
    pub fn from_addr(addr: SocketAddr) -> Self {
        Self {
            intercept_port: addr.port(),
            intercept_url: format!("http://{addr}"),
            ca_cert_pem: None,
            ca_key_pem: None,
        }
    }

    /// Build the start-response status, attaching the CA export when the start produced one.
    pub fn from_started(started: StartedIntercept) -> Self {
        let mut status = Self::from_addr(started.addr);
        if let Some((cert, key)) = started.ca_export {
            status.ca_cert_pem = Some(cert);
            status.ca_key_pem = Some(key);
        }
        status
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
    /// Seeding the start-time rules exceeded [`MAX_RULES`](crate::intercept_rules::MAX_RULES).
    /// Mapped to the same `429` the runtime `POST /intercept/rules` returns for a full store.
    #[error(transparent)]
    Rules(#[from] RulesAtCapacity),
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
    ) -> Result<StartedIntercept, InterceptStartError> {
        if self.is_occupied() {
            return Err(InterceptStartError::AlreadyRunning);
        }

        let host = opts.host.as_deref().unwrap_or("127.0.0.1");
        let addr: SocketAddr = format!("{host}:{}", opts.port.unwrap_or(0))
            .parse()
            .map_err(|e| InterceptStartError::InvalidAddr(format!("{e}")))?;

        // Resolve the single CA source (validating both-or-neither + pair exclusion) before binding.
        // Log CA/option failures here so every surface (FFI, admin `POST /intercept`, CLI flag) gets
        // a server-side trail — not just the FFI, which used to `warn!` these on its own. The map to
        // `Ca` keeps these validation failures on the existing 400 path.
        let source = CaSource::resolve(
            opts.ca_cert_path.map(PathBuf::from),
            opts.ca_key_path.map(PathBuf::from),
            opts.ca_cert_pem,
            opts.ca_key_pem,
        )
        .map_err(|e| {
            tracing::warn!(error = %e, "intercept start: invalid CA options");
            InterceptStartError::Ca(e)
        })?;

        // `returnCaKey` hands the CA private key back to the caller, so it is only allowed against a
        // CA this call generates (issue #593, D4). Allowing it with a supplied path/PEM source would
        // turn the admin API into a file-exfiltration primitive (echo back any keypair on disk).
        let return_ca_key = opts.return_ca_key.unwrap_or(false);
        if return_ca_key && !source.is_generate() {
            return Err(InterceptStartError::Ca(anyhow::anyhow!(
                "returnCaKey requires the CA to be generated by this call (omit caCert*/caKey*)"
            )));
        }

        let ca = Arc::new(CertificateAuthority::from_source(&source).map_err(|e| {
            tracing::warn!(error = %e, "intercept start: CA setup failed");
            InterceptStartError::Ca(e)
        })?);
        let ca_export = return_ca_key.then(|| (ca.ca_cert_pem().to_string(), ca.ca_key_pem()));

        // Seed BEFORE binding: once `bind` returns the listener is accepting, so rules installed
        // after it could be raced by the first request, which would fall through to the
        // unmatched-host default (issue #655). Seeding here makes a started listener correct by
        // construction — and an over-capacity batch fails the start rather than binding a listener
        // with a partial rule set.
        let rules = InterceptRules::new();
        rules.extend(opts.rules).map_err(|e| {
            tracing::warn!(error = %e, "intercept start: rule seeding failed");
            InterceptStartError::Rules(e)
        })?;

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
            Ok(()) => Ok(StartedIntercept {
                addr: bound,
                ca_export,
            }),
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

        let started = control.start(defaults()).await.expect("start");
        assert_eq!(control.status(), Some(started.addr));
        assert!(started.addr.port() > 0, "OS assigned a real port");
        assert!(
            started.ca_export.is_none(),
            "no CA export unless returnCaKey was requested"
        );
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
        assert_eq!(
            installed, won.addr,
            "the installed listener is the winner's"
        );
        control.stop().await;
    }

    // Issue #593: the manual Debug impls must never render CA key material — guard against a future
    // field being added without updating the redaction.
    #[test]
    fn debug_impls_redact_ca_key_material() {
        let secret = "supersecret-key-bytes";
        let opts = InterceptStartOptions {
            ca_cert_pem: Some("cert".to_string()),
            ca_key_pem: Some(secret.to_string()),
            ..Default::default()
        };
        assert!(
            !format!("{opts:?}").contains(secret),
            "InterceptStartOptions Debug must redact the inline key PEM"
        );
        let started = StartedIntercept {
            addr: "127.0.0.1:0".parse().unwrap(),
            ca_export: Some(("cert".to_string(), secret.to_string())),
        };
        assert!(
            !format!("{started:?}").contains(secret),
            "StartedIntercept Debug must redact the exported CA key"
        );
    }

    // Issue #593: a CA pair for inline-PEM tests, minted out of band.
    fn ca_pem_pair() -> (String, String) {
        let ca = CertificateAuthority::generate().expect("generate CA out of band");
        (ca.ca_cert_pem().to_string(), ca.ca_key_pem())
    }

    #[tokio::test]
    async fn start_with_inline_pem_loads_ca() {
        let (cert_pem, key_pem) = ca_pem_pair();
        let control = InterceptControl::default();
        let opts = InterceptStartOptions {
            ca_cert_pem: Some(cert_pem.clone()),
            ca_key_pem: Some(key_pem),
            ..Default::default()
        };
        control.start(opts).await.expect("start with inline PEM");
        let state = control.state().expect("running");
        assert_eq!(
            state.ca.ca_cert_pem(),
            cert_pem,
            "the running listener uses the supplied CA as its trust anchor"
        );
        control.stop().await;
    }

    #[tokio::test]
    async fn return_ca_key_exports_generated_pair() {
        let control = InterceptControl::default();
        let opts = InterceptStartOptions {
            return_ca_key: Some(true),
            ..Default::default()
        };
        let started = control.start(opts).await.expect("start with returnCaKey");
        let (cert, key) = started.ca_export.expect("CA pair returned");
        assert!(cert.contains("CERTIFICATE") && key.contains("PRIVATE KEY"));
        // The returned pair reconstructs the same running CA.
        assert_eq!(control.state().unwrap().ca.ca_cert_pem(), cert);
        let reloaded = CertificateAuthority::load_pem(&cert, &key).expect("reload returned pair");
        assert_eq!(
            reloaded.ca_cert_pem(),
            cert,
            "returned pair is a usable anchor"
        );
        control.stop().await;
    }

    #[tokio::test]
    async fn return_ca_key_with_supplied_source_is_error() {
        // With inline PEM.
        let (cert_pem, key_pem) = ca_pem_pair();
        let control = InterceptControl::default();
        let err = control
            .start(InterceptStartOptions {
                ca_cert_pem: Some(cert_pem),
                ca_key_pem: Some(key_pem),
                return_ca_key: Some(true),
                ..Default::default()
            })
            .await
            .expect_err("returnCaKey with a supplied CA must be rejected");
        assert!(matches!(err, InterceptStartError::Ca(_)));
        assert!(control.status().is_none(), "no listener left behind");

        // With CA paths.
        let err = control
            .start(InterceptStartOptions {
                ca_cert_path: Some("cert.pem".to_string()),
                ca_key_path: Some("key.pem".to_string()),
                return_ca_key: Some(true),
                ..Default::default()
            })
            .await
            .expect_err("returnCaKey with CA paths must be rejected");
        assert!(matches!(err, InterceptStartError::Ca(_)));
    }

    // ===== Config-declared rules seeded at start (issue #655) =====

    fn serve_rule(host: &str) -> InterceptRule {
        InterceptRule {
            host: Some(host.to_string()),
            predicates: vec![],
            action: crate::intercept_rules::InterceptAction::Serve(
                crate::intercept_rules::ServeStub {
                    status_code: 200,
                    headers: Default::default(),
                    body: Some("seeded".to_string()),
                },
            ),
        }
    }

    /// AC1: rules supplied to `start` are in the store the listener matches against by the time
    /// `start` returns — no second admin call, and (because seeding precedes `bind`) no window in
    /// which the listener is live with an empty store.
    #[tokio::test]
    async fn start_seeds_rules_from_options() {
        let control = InterceptControl::default();
        control
            .start(InterceptStartOptions {
                rules: vec![serve_rule("a.example.com"), serve_rule("b.example.com")],
                ..Default::default()
            })
            .await
            .expect("start with seeded rules");

        let listed = control.state().expect("running").rules.list();
        assert_eq!(listed.len(), 2, "both config rules are in the live store");
        assert_eq!(listed[0].host.as_deref(), Some("a.example.com"));
        assert_eq!(
            listed[1].host.as_deref(),
            Some("b.example.com"),
            "insertion order is preserved (first match wins depends on it)"
        );
        control.stop().await;
    }

    /// AC2 (unit half): an options payload without `rules` behaves exactly as before.
    #[tokio::test]
    async fn start_without_rules_leaves_store_empty() {
        let control = InterceptControl::default();
        control.start(defaults()).await.expect("start");
        assert!(
            control.state().expect("running").rules.is_empty(),
            "no rules unless the caller supplied some"
        );
        control.stop().await;
    }

    /// AC6: runtime `POST /intercept/rules` / `DELETE` still layer on top of the seeded set.
    #[tokio::test]
    async fn seeded_rules_accept_runtime_additions_and_clear() {
        let control = InterceptControl::default();
        control
            .start(InterceptStartOptions {
                rules: vec![serve_rule("seeded.example.com")],
                ..Default::default()
            })
            .await
            .expect("start");
        let rules = control.state().expect("running").rules;

        rules
            .add(serve_rule("runtime.example.com"))
            .expect("runtime add on top of the seeded set");
        let listed = rules.list();
        assert_eq!(listed.len(), 2, "runtime rule layers on top, not replacing");
        assert_eq!(listed[0].host.as_deref(), Some("seeded.example.com"));
        assert_eq!(listed[1].host.as_deref(), Some("runtime.example.com"));

        rules.clear();
        assert!(rules.is_empty(), "DELETE clears seeded rules too");
        control.stop().await;
    }

    /// Seeding failure fails `start` loudly (and therefore server boot) rather than binding a
    /// listener with a partial rule set.
    #[tokio::test]
    async fn start_rejects_rules_over_capacity_without_binding() {
        let control = InterceptControl::default();
        let too_many = (0..crate::intercept_rules::MAX_RULES + 1)
            .map(|i| serve_rule(&format!("h{i}.example.com")))
            .collect();
        let err = control
            .start(InterceptStartOptions {
                rules: too_many,
                ..Default::default()
            })
            .await
            .expect_err("an over-capacity rule set must fail the start");
        assert!(matches!(err, InterceptStartError::Rules(_)));
        assert!(
            control.status().is_none(),
            "no listener may be left behind by a failed seed"
        );
    }

    /// The config block IS this struct: `rules` must parse from the same camelCase JSON the admin
    /// `POST /intercept` body uses, and stay optional for pre-#655 payloads.
    #[test]
    fn rules_parse_from_json_and_default_to_empty() {
        let seeded: InterceptStartOptions = serde_json::from_str(
            r#"{"port":8080,"rules":[{"host":"cdn.example.com","action":{"forward":{"port":4545}}}]}"#,
        )
        .expect("rules parse from the shared camelCase shape");
        assert_eq!(seeded.rules.len(), 1);
        assert_eq!(seeded.port, Some(8080));

        let legacy: InterceptStartOptions =
            serde_json::from_str(r#"{"port":8080}"#).expect("a pre-#655 payload still parses");
        assert!(legacy.rules.is_empty(), "rules defaults to empty");
    }

    #[tokio::test]
    async fn inline_pem_half_pair_and_mutual_exclusion_are_errors() {
        let control = InterceptControl::default();
        // Half a PEM pair.
        let err = control
            .start(InterceptStartOptions {
                ca_cert_pem: Some("cert".to_string()),
                ..Default::default()
            })
            .await
            .expect_err("half PEM pair");
        assert!(matches!(err, InterceptStartError::Ca(_)));

        // PEM pair AND path pair.
        let (cert_pem, key_pem) = ca_pem_pair();
        let err = control
            .start(InterceptStartOptions {
                ca_cert_path: Some("cert.pem".to_string()),
                ca_key_path: Some("key.pem".to_string()),
                ca_cert_pem: Some(cert_pem),
                ca_key_pem: Some(key_pem),
                ..Default::default()
            })
            .await
            .expect_err("path and PEM are mutually exclusive");
        assert!(matches!(err, InterceptStartError::Ca(_)));
        assert!(control.status().is_none());
    }
}
