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

use crate::admin_api::{AdminExposurePolicy, check_intercept_exposure};
use crate::intercept::InterceptListener;
use crate::intercept_rules::{InterceptRule, InterceptRules, InterceptState, RulesAtCapacity};
use rift_mock_core::proxy::OutboundTls;
use rift_mock_core::proxy::intercept_ca::{CaSource, CertificateAuthority, SniCertResolver};
use serde::Serialize;

/// Log an `anyhow`-typed intercept-start failure with its whole cause chain (issue #683).
///
/// `{e}` renders only the outermost context, so "CA setup failed" read identically whether the PEM
/// was malformed, the key mismatched, or the file was unreadable. The `&anyhow::Error` parameter is
/// load-bearing: it makes the type rule compile-enforced, so the sibling `rule seeding failed` site
/// — whose error is a concrete `RulesAtCapacity` with no chain to render — cannot be routed through
/// here by mistake.
fn warn_intercept_start_failure(e: &anyhow::Error, what: &str) {
    tracing::warn!(error = %format_args!("{e:#}"), "intercept start: {what}");
}

/// A running intercept plane: the listener plus the control-plane [`InterceptState`] (rule store +
/// CA) the admin routes and FFI mutate/export.
pub struct InterceptPlane {
    pub listener: InterceptListener,
    pub state: InterceptState,
    /// Whether this listener demands a credential. Kept so a policy stated *after* the listener came
    /// up can still be applied to it (issue #1149) — see
    /// [`InterceptControl::check_running_exposure`].
    pub has_auth: bool,
}

/// Why [`InterceptControl::install`] would not take the slot. Carries the listener back so the
/// caller can shut it down — a bound listener must never be dropped without being stopped.
enum InstallRefused {
    /// Another start won the race for the slot.
    AlreadyRunning(InterceptListener),
    /// The exposure policy became `Refuse` while this listener was binding (issue #1149).
    Exposed(InterceptListener, String),
}

/// Shared, mutable slot for the process's (or FFI handle's) single intercept plane. Cheap to clone
/// (an `Arc` inside). The `std` mutex is never held across an `.await` (see [`InterceptControl::start`]);
/// poisoning is recovered rather than propagated, like [`InterceptRules`].
#[derive(Clone, Default)]
pub struct InterceptControl {
    plane: Arc<Mutex<Option<InterceptPlane>>>,
    /// Shared like `plane`, and for a reason the by-value form could not satisfy (issue #1149): the
    /// FFI builds the control in `rift_start()` and hands clones to the admin server long before
    /// `rift_serve_admin` learns `requireAdminAuth`, so a policy stored per clone would be fixed at
    /// its default before the operator ever stated one.
    policy: Arc<Mutex<InterceptPolicy>>,
}

/// The operator's deployment policy for this control's listener.
///
/// Both fields are the operator's call rather than the caller's, which is why they live here and
/// not on [`InterceptStartOptions`] — that struct is also the `POST /intercept` request body, and
/// putting the policy in it would let the very caller being judged turn the judgement off.
#[derive(Debug, Clone, Default)]
struct InterceptPolicy {
    /// What to do when a start would expose this listener off-host with no credential (issue #878).
    exposure: AdminExposurePolicy,
    /// Trust for connections this listener makes *outbound*, to a real origin (issue #997). The
    /// untouched default means system roots only, which is what a listener started before this
    /// existed effectively had.
    outbound_tls: OutboundTls,
}

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
    /// Require `Proxy-Authorization: Basic <base64(user:pass)>` on `CONNECT` (issue #878).
    /// `None` leaves the listener open, which is what it has always been — auth is opt-in because
    /// the listener is off unless asked for and most uses are loopback test rigs.
    pub auth: Option<InterceptAuth>,
    /// Rules to install before the listener accepts anything (issue #655). Lets one declarative
    /// document — a `--configfile` `intercept` block, a `POST /intercept` body, or an FFI start —
    /// bring up a listener that is already correct, instead of requiring a follow-up
    /// `POST /intercept/rules` that traffic can race.
    #[serde(default)]
    pub rules: Vec<InterceptRule>,
}

/// The credential the intercept proxy demands (issue #878).
///
/// Basic only. It is what every HTTP client and every `HTTPS_PROXY=http://user:pass@host` URL
/// already speaks, and the tunnel it guards is plaintext to the client anyway — a stronger scheme
/// here would imply a confidentiality this hop cannot provide.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterceptAuth {
    pub username: String,
    pub password: String,
}

impl InterceptAuth {
    /// Reject a blank username or password, mirroring `validate_admin_api_key` (issue #844): a
    /// blank secret switches the gate on and then admits everyone, which is strictly worse than
    /// no gate because the operator believes one is in force. Whitespace-only counts as blank.
    pub fn validate(&self) -> Result<(), String> {
        if self.username.trim().is_empty() || self.password.trim().is_empty() {
            return Err(
                "intercept proxy auth needs a non-blank username and password; a blank value \
                 would enable the gate and then accept every request. Omit it entirely to run the \
                 intercept listener explicitly unauthenticated."
                    .to_string(),
            );
        }
        Ok(())
    }
}

impl std::fmt::Debug for InterceptAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secret material — the username is shown because it is an identity, the password never is.
        f.debug_struct("InterceptAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
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
            .field("auth", &self.auth)
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
    /// A supplied proxy credential is unusable (issue #878). Its own variant rather than folded
    /// into `InvalidAddr` so the admin API's 400 names the real problem.
    #[error("invalid intercept auth: {0}")]
    InvalidAuth(String),
    /// The start would expose the listener off-host with no credential, under a `Refuse` policy
    /// (issue #878). Distinct from `InvalidAuth`: the options are well-formed, the *posture* is not.
    #[error("{0}")]
    Exposed(String),
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
    /// Set the exposure policy every [`start`](Self::start) through this control is judged against
    /// (issue #878). The standalone binary threads `--require-admin-auth` in here, so a listener
    /// brought up at runtime over `POST /intercept` gets the same answer as one asked for at boot.
    ///
    /// The policy is shared across clones (issue #1149), so the order of configure-vs-clone does
    /// not matter.
    #[must_use]
    pub fn with_exposure_policy(self, policy: AdminExposurePolicy) -> Self {
        self.set_exposure_policy(policy);
        self
    }

    /// Replace the exposure policy every later [`start`](Self::start) through this control — and
    /// through every clone of it — is judged by.
    ///
    /// The setter form exists for callers that only ever hold a clone, which is the FFI's shape: the
    /// control is created in `rift_start()` and the policy is not known until `rift_serve_admin`
    /// (issue #1149).
    pub fn set_exposure_policy(&self, policy: AdminExposurePolicy) {
        self.policy_lock().exposure = policy;
    }

    /// Set the outbound TLS trust every listener started through this control uses when it relays
    /// to a real origin (issue #997 — WebSocket passthrough is the first thing that does).
    ///
    /// Threaded from the process-wide policy so an intercept relay trusts exactly what an imposter
    /// `proxy` stub trusts. Divergent per-client trust is what #974 was filed to remove, and a
    /// private-CA origin — the common case for the systems an intercept proxy is pointed at —
    /// works only if this is shared rather than re-derived.
    ///
    /// Shared across clones, like the exposure policy.
    #[must_use]
    pub fn with_outbound_tls(self, outbound_tls: OutboundTls) -> Self {
        self.set_outbound_tls(outbound_tls);
        self
    }

    /// Replace the outbound trust every later [`start`](Self::start) through this control uses.
    ///
    /// Only affects listeners started afterwards: a listener builds its origin TLS config at bind,
    /// so one already running keeps the trust it was born with (issue #1149).
    pub fn set_outbound_tls(&self, outbound_tls: OutboundTls) {
        self.policy_lock().outbound_tls = outbound_tls;
    }

    /// The outbound trust currently configured on this control.
    #[must_use]
    pub fn outbound_tls(&self) -> OutboundTls {
        self.policy_lock().outbound_tls.clone()
    }

    fn policy_lock(&self) -> std::sync::MutexGuard<'_, InterceptPolicy> {
        self.policy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of the policy, taken in a short sync scope so the (`!Send`) `std` guard never spans an
    /// `.await` — the same rule [`lock`](Self::lock) follows.
    fn policy(&self) -> InterceptPolicy {
        self.policy_lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<InterceptPlane>> {
        self.plane.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True if a listener currently occupies the slot. A sync helper so the (`!Send`) `std`
    /// mutex guard never spans an `.await` in [`start`](Self::start).
    fn is_occupied(&self) -> bool {
        self.lock().is_some()
    }

    /// Install a freshly-bound plane, or hand the listener back if the slot filled in the meantime
    /// (a concurrent start won the race). Sync so the guard is confined to this frame.
    /// Take the slot, re-judging the exposure policy **under the lock**.
    ///
    /// `start` snapshots the policy before it `.await`s the bind, which leaves a window exactly as
    /// wide as that bind: a concurrent `set_exposure_policy(Refuse)` — `rift_serve_admin` on another
    /// thread, which the SDKs really do drive concurrently — would see an empty slot in
    /// [`check_running_exposure`], report success, and then this listener would install anyway. That
    /// is the posture issue #1149 exists to prevent, so the last word is taken here rather than at
    /// the snapshot.
    ///
    /// Lock order is `plane` then `policy`, and nothing takes them the other way round: every other
    /// `policy_lock()` is a statement-scoped temporary, and `check_running_exposure` copies the
    /// policy out before it touches `plane`.
    fn install(&self, plane: InterceptPlane) -> Result<(), InstallRefused> {
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(InstallRefused::AlreadyRunning(plane.listener));
        }
        let exposure = self.policy_lock().exposure;
        if let Err(e) =
            check_intercept_exposure(plane.listener.local_addr(), plane.has_auth, exposure)
        {
            return Err(InstallRefused::Exposed(plane.listener, format!("{e:#}")));
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
        let addr =
            rift_mock_core::proxy::bind_addr(host, opts.port.unwrap_or(0)).ok_or_else(|| {
                InterceptStartError::InvalidAddr(format!(
                    "`{host}` is not an IP literal (IPv4, or IPv6 bare `::1` or bracketed `[::1]`)"
                ))
            })?;

        // Validate the credential before any side effect: a blank secret must never reach the
        // listener, where it would switch the gate on and then admit everyone (issue #878/#844).
        let auth = opts.auth;
        if let Some(ref auth) = auth {
            auth.validate().map_err(InterceptStartError::InvalidAuth)?;
        }
        let auth_required = auth.is_some();

        // Judge the exposure here, at the one point all four doors converge, rather than only at
        // the CLI ones (issue #878). `POST /intercept` and `rift_start_intercept` can bring up a
        // listener long after boot: checking only at startup would let an operator who set
        // `--require-admin-auth` still end up with an open MITM proxy on `0.0.0.0`, which is
        // precisely the assurance that flag is supposed to give. Before any side effect, so a
        // refusal never has to unwind a bound listener.
        let policy = self.policy();
        check_intercept_exposure(addr, auth.is_some(), policy.exposure)
            .map_err(|e| InterceptStartError::Exposed(format!("{e:#}")))?;

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
            warn_intercept_start_failure(&e, "invalid CA options");
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
            warn_intercept_start_failure(&e, "CA setup failed");
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
        let listener =
            InterceptListener::bind(addr, resolver, rules.clone(), auth, policy.outbound_tls)
                .await
                .map_err(|e| {
                    warn_intercept_start_failure(&e, "bind failed");
                    InterceptStartError::Bind(e)
                })?;
        let bound = listener.local_addr();

        match self.install(InterceptPlane {
            listener,
            state: InterceptState { rules, ca },
            has_auth: auth_required,
        }) {
            Ok(()) => Ok(StartedIntercept {
                addr: bound,
                ca_export,
            }),
            Err(InstallRefused::AlreadyRunning(listener)) => {
                listener.shutdown().await;
                Err(InterceptStartError::AlreadyRunning)
            }
            // The policy turned strict while this listener was binding. Shut it down and report the
            // refusal the caller would have got had the policy been set a moment earlier.
            Err(InstallRefused::Exposed(listener, reason)) => {
                listener.shutdown().await;
                Err(InterceptStartError::Exposed(reason))
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

    /// Re-judge a listener that is **already running** against the current exposure policy
    /// (issue #1149).
    ///
    /// A listener started before the policy was stated was judged under the default `Warn`. If the
    /// operator then states `requireAdminAuth`, returning success would make that option's promise
    /// false for a listener that is exposed right now. `Ok(())` when nothing is running, when the
    /// policy is not `Refuse`, or when the running listener passes.
    ///
    /// Deliberately does **not** stop the listener: unwinding is reserved for a listener the failing
    /// call started itself.
    pub fn check_running_exposure(&self) -> anyhow::Result<()> {
        let exposure = self.policy_lock().exposure;
        if exposure != AdminExposurePolicy::Refuse {
            return Ok(());
        }
        let Some((addr, has_auth)) = self
            .lock()
            .as_ref()
            .map(|p| (p.listener.local_addr(), p.has_auth))
        else {
            return Ok(());
        };
        check_intercept_exposure(addr, has_auth, exposure)
    }

    /// A clone of the running plane's [`InterceptState`] for the rules/CA/truststore handlers.
    /// Cheap: `InterceptState` is `Arc`-backed rules + an `Arc` CA.
    pub fn state(&self) -> Option<InterceptState> {
        self.lock().as_ref().map(|p| p.state.clone())
    }
}

#[cfg(test)]
mod anyhow_chain_log_tests {
    use super::warn_intercept_start_failure;
    use tracing_test::traced_test;

    fn chained_error() -> anyhow::Error {
        anyhow::anyhow!("PEM section is not a private key")
            .context("failed to read CA key")
            .context("CA setup failed")
    }

    #[traced_test]
    #[test]
    fn warn_intercept_start_failure_names_the_whole_chain() {
        warn_intercept_start_failure(&chained_error(), "CA setup failed");
        assert!(
            logs_contain("intercept start: CA setup failed"),
            "message survives"
        );
        assert!(
            logs_contain("failed to read CA key"),
            "the log must name the CAUSE — the whole point of #683"
        );
        assert!(
            logs_contain("PEM section is not a private key"),
            "the log must reach the ROOT cause, not stop one level down"
        );
    }

    // Documents the defect #683 fixes, so the sibling test above reads as a real assertion and not
    // a tautology: plain Display stops at the outermost context, and `{:#}` is what recovers the rest.
    #[test]
    fn plain_display_drops_the_chain() {
        let rendered = format!("{}", chained_error());
        assert_eq!(rendered, "CA setup failed");
        assert!(
            !rendered.contains("PEM section"),
            "plain Display must NOT reach the root cause; if it did, #683 would be a non-issue"
        );
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
                crate::intercept_rules::ServeStub::new(
                    200,
                    Default::default(),
                    Some(serde_json::json!("seeded")),
                ),
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

    // Issue #1149: the policy used to live by value on the control, so a clone taken before it was
    // configured kept the default `Warn` for ever. That is exactly the FFI's shape — `rift_start()`
    // builds the control and hands clones to the admin server long before `rift_serve_admin` learns
    // `requireAdminAuth` — so the policy has to be shared state, not a field on each clone.
    #[tokio::test]
    async fn a_clone_taken_before_the_policy_is_set_still_refuses_an_exposed_start() {
        let control = InterceptControl::default();
        let clone_taken_early = control.clone();

        control.set_exposure_policy(AdminExposurePolicy::Refuse);

        let err = clone_taken_early
            .start(InterceptStartOptions {
                host: Some("0.0.0.0".to_string()),
                port: Some(0),
                ..Default::default()
            })
            .await
            .expect_err("an off-host start with no credential must be refused under Refuse");
        assert!(
            matches!(err, InterceptStartError::Exposed(_)),
            "expected Exposed, got {err:?}"
        );
        assert!(
            clone_taken_early.status().is_none(),
            "a refused start must leave nothing bound"
        );
    }

    // The policy is whatever was set most recently, not a high-water mark. Pinned because "sticky"
    // is the other plausible reading, and it would let a refusal outlive the configuration that
    // asked for it — the FFI sets this on every `rift_serve_admin`, including ones that omit the
    // option (issue #1149).
    #[tokio::test]
    async fn setting_the_policy_back_to_warn_takes_effect() {
        let control = InterceptControl::default();
        control.set_exposure_policy(AdminExposurePolicy::Refuse);
        control.set_exposure_policy(AdminExposurePolicy::Warn);

        let started = control
            .start(InterceptStartOptions {
                host: Some("0.0.0.0".to_string()),
                port: Some(0),
                ..Default::default()
            })
            .await
            .expect("Warn only warns; the start must succeed");
        assert!(started.addr.port() > 0);
        assert!(control.status().is_some());
    }

    // A listener that came up under `Warn` is re-judged when the policy later becomes `Refuse` —
    // the retrofit that makes `requireAdminAuth` mean something for an already-running listener.
    #[tokio::test]
    async fn check_running_exposure_refuses_a_listener_started_before_the_policy() {
        let control = InterceptControl::default();
        control
            .start(InterceptStartOptions {
                host: Some("0.0.0.0".to_string()),
                port: Some(0),
                ..Default::default()
            })
            .await
            .expect("an exposed start under the default Warn");
        assert!(
            control.check_running_exposure().is_ok(),
            "under Warn there is nothing to refuse"
        );

        control.set_exposure_policy(AdminExposurePolicy::Refuse);
        assert!(
            control.check_running_exposure().is_err(),
            "an exposed listener must be refused once the policy says Refuse"
        );
        assert!(
            control.status().is_some(),
            "checking must not stop the listener"
        );
    }

    // A credentialled listener passes the same re-judgement: the gate is authentication, not the
    // address.
    #[tokio::test]
    async fn check_running_exposure_accepts_a_credentialled_listener() {
        let control = InterceptControl::default();
        control
            .start(InterceptStartOptions {
                host: Some("0.0.0.0".to_string()),
                port: Some(0),
                auth: Some(InterceptAuth {
                    username: "u".to_string(),
                    password: "p".to_string(),
                }),
                ..Default::default()
            })
            .await
            .expect("a credentialled off-host start");

        control.set_exposure_policy(AdminExposurePolicy::Refuse);
        assert!(control.check_running_exposure().is_ok());
    }

    // Nothing running is nothing to judge.
    #[tokio::test]
    async fn check_running_exposure_is_ok_with_no_listener() {
        let control = InterceptControl::default();
        control.set_exposure_policy(AdminExposurePolicy::Refuse);
        assert!(control.check_running_exposure().is_ok());
    }

    /// An exposed, credential-less plane bound on `0.0.0.0`, built the way `start` builds one but
    /// without going through `start` — so a test can hand it to `install` at a moment of its choosing.
    async fn exposed_plane() -> InterceptPlane {
        let ca = Arc::new(CertificateAuthority::generate().expect("generate CA"));
        let rules = InterceptRules::new();
        let listener = InterceptListener::bind(
            "0.0.0.0:0".parse().expect("addr"),
            Arc::new(SniCertResolver::new(ca.clone())),
            rules.clone(),
            None,
            OutboundTls::default(),
        )
        .await
        .expect("bind");
        InterceptPlane {
            listener,
            state: InterceptState { rules, ca },
            has_auth: false,
        }
    }

    // The TOCTOU window (issue #1149). `start` snapshots the policy before it awaits the bind, so a
    // concurrent `set_exposure_policy(Refuse)` — `rift_serve_admin` on another thread — would see an
    // empty slot, pass `check_running_exposure`, and report success while this listener installed
    // itself anyway. Racing a real bind is not deterministic, so this reproduces the interleaving
    // directly: the plane is bound under `Warn`, the policy turns strict, *then* install runs.
    #[tokio::test]
    async fn install_refuses_an_exposed_plane_if_the_policy_turned_strict_during_the_bind() {
        let control = InterceptControl::default();
        let plane = exposed_plane().await;

        control.set_exposure_policy(AdminExposurePolicy::Refuse);

        match control.install(plane) {
            Err(InstallRefused::Exposed(listener, _)) => listener.shutdown().await,
            Err(InstallRefused::AlreadyRunning(listener)) => {
                listener.shutdown().await;
                panic!("the slot was empty; this must be refused as exposed, not as a race");
            }
            Ok(()) => panic!("an exposed plane must not install once the policy is Refuse"),
        }
        assert!(
            control.status().is_none(),
            "a refused install must leave the slot empty"
        );
    }

    // And the same plane installs fine while the policy is still `Warn` — so the guard above is
    // about the policy, not about the plane.
    #[tokio::test]
    async fn install_accepts_an_exposed_plane_under_warn() {
        let control = InterceptControl::default();
        let plane = exposed_plane().await;
        assert!(control.install(plane).is_ok());
        assert!(control.status().is_some());
        control.stop().await;
    }

    // The outbound trust is shared the same way and for the same reason.
    #[tokio::test]
    async fn a_clone_sees_outbound_tls_set_after_it_was_taken() {
        let control = InterceptControl::default();
        let clone_taken_early = control.clone();
        assert!(!clone_taken_early.outbound_tls().is_configured());

        control.set_outbound_tls(OutboundTls {
            ca_pem: Some("-----BEGIN CERTIFICATE-----".to_string()),
            skip_verify: false,
        });

        assert!(
            clone_taken_early.outbound_tls().is_configured(),
            "the clone must see the trust the control was given afterwards"
        );
    }
}
