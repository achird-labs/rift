//! Admin API server.

use crate::admin_api::authz;
use crate::admin_api::handlers::events::{self, AdminBody};
use crate::admin_api::router::route_request;
use crate::config_loader::ConfigSource;
use crate::extensions::decorate::{ResponsePhase, with_annotation_scope};
use crate::imposter::ImposterManager;
use crate::intercept_control::InterceptControl;
use crate::sources::{ReloadSource, SourceSet};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use rift_mock_core::extensions::authz::{
    AdminAuthorizer, AuthzDecision, AuthzRequest, with_principal_scope,
};
use rift_mock_core::proxy::{
    AcceptBackoff, AcceptErrorClass, AcceptErrorEvent, AcceptErrorLog, classify_accept_error,
    is_fatal_listener_error,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, warn};

/// Bounded grace given to in-flight connections on `shutdown()` before the wait is abandoned.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Header carrying the embedder-defined scope selector handed to an [`AdminAuthorizer`]
/// (issue #854). Upstream never parses or interprets the value — it exists because an authorizer
/// often cannot derive the target from the port alone (`POST /imposters` has no port yet).
const SCOPE_HEADER: &str = "x-rift-scope";

/// Admin API server for Rift
pub struct AdminApiServer {
    addr: SocketAddr,
    manager: Arc<ImposterManager>,
    api_key: Option<Arc<String>>,
    config_source: Option<ReloadSource>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
    /// `--local-only` as supplied, reported verbatim by `GET /config` (issue #879). The flag, not
    /// "did we bind loopback" — see `ConfigSnapshot::local_only`.
    local_only: bool,
    authorizer: Option<Arc<dyn AdminAuthorizer>>,
    /// Exposure policy for [`bind`](Self::bind) (issue #863), or `None` when an outer door already
    /// ran the check — see [`with_exposure_checked`](Self::with_exposure_checked).
    exposure: Option<AdminExposurePolicy>,
}

impl AdminApiServer {
    /// Create a new admin API server
    pub fn new(addr: SocketAddr, manager: Arc<ImposterManager>, api_key: Option<String>) -> Self {
        Self {
            addr,
            manager,
            api_key: api_key.map(Arc::new),
            config_source: None,
            allow_injection: false,
            intercept: None,
            scripts_dir: None,
            authorizer: None,
            local_only: false,
            exposure: Some(AdminExposurePolicy::default()),
        }
    }

    /// Record `--local-only` for `GET /config` to report (issue #879). Threaded explicitly, like
    /// [`with_allow_injection`](Self::with_allow_injection), so an embedder states it rather than
    /// having it inferred from the bind address — which would overstate the restriction, since the
    /// metrics and imposter listeners are governed separately.
    #[must_use]
    pub fn with_local_only(mut self, local_only: bool) -> Self {
        self.local_only = local_only;
        self
    }

    /// Refuse to bind when this server would be reachable off-host with no API key, instead of
    /// warning (issue #863). The embedder-facing spelling of `--require-admin-auth`; `false` (the
    /// default) keeps the warning.
    #[must_use]
    pub fn with_require_admin_auth(mut self, require: bool) -> Self {
        self.exposure = Some(require.into());
        self
    }

    /// Suppress this server's own exposure check because an outer door already ran it (issue #863).
    ///
    /// The CLI and the C-ABI both have to run [`check_admin_exposure`] *before* they bind anything
    /// else — under `Refuse` the error must not have to unwind an already-bound metrics listener —
    /// so without this the same startup would emit the warning twice. Not for embedders building an
    /// `AdminApiServer` directly: for them `bind()` is the only door, and it must do the checking.
    ///
    /// Hidden from the rendered docs deliberately — it is a "skip the check" switch, public only
    /// because `rift-ffi` is a separate crate and needs to reach it. Order-sensitive: calling it
    /// after [`with_require_admin_auth`](Self::with_require_admin_auth) discards that strictness.
    #[doc(hidden)]
    #[must_use]
    pub fn with_exposure_checked(mut self) -> Self {
        self.exposure = None;
        self
    }

    /// Install a per-request authorization hook (issue #854).
    ///
    /// Consulted after authentication and after the route is parsed, so it receives the action,
    /// port and path params rather than a raw path. Without one the api-key comparison decides
    /// alone, exactly as before.
    #[must_use]
    pub fn with_admin_authorizer(mut self, authorizer: Arc<dyn AdminAuthorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Set the config source (`--configfile`/`--datadir`) so `POST /admin/reload` can re-read it
    /// (issue #197). Without it, reload is a no-op.
    #[must_use]
    pub fn with_config_source(mut self, source: ConfigSource) -> Self {
        self.config_source = Some(ReloadSource::Legacy(Arc::new(source)));
        self
    }

    /// Set the `--imposters` source set so `POST /admin/reload` re-fetches every source (U-12).
    /// Replaces any previously-set config source: a server reloads from one place.
    #[must_use]
    pub fn with_imposter_sources(mut self, sources: Arc<SourceSet>) -> Self {
        self.config_source = Some(ReloadSource::Sources(sources));
        self
    }

    /// Set whether JS injection is allowed, reported by `GET /config` (issue #342). Threaded
    /// explicitly so an embedder can set it without mutating the process environment.
    #[must_use]
    pub fn with_allow_injection(mut self, allow: bool) -> Self {
        self.allow_injection = allow;
        self
    }

    /// Wire the `/intercept` admin routes to the shared [`InterceptControl`] slot: the runtime
    /// lifecycle verbs (`POST`/`GET`/`DELETE /intercept`, issue #493) plus rule CRUD + CA/truststore
    /// export (epic #394 slice 4). The control may be empty (no listener yet) — the lifecycle
    /// endpoints still work and can start one. Without this call, all of `/intercept*` responds
    /// `404` — the admin server has no intercept surface unless an embedder explicitly opts in.
    #[must_use]
    pub fn with_intercept(mut self, control: InterceptControl) -> Self {
        self.intercept = Some(control);
        self
    }

    /// Set the root directory `_rift.script` `file:` references resolve under for imposters
    /// created through the admin API (issue #356). Without it, admin-API `file:` references are
    /// rejected — see `imposter::ScriptBaseDir::Unconfigured`.
    #[must_use]
    pub fn with_scripts_dir(mut self, dir: PathBuf) -> Self {
        self.scripts_dir = Some(Arc::new(dir));
        self
    }

    /// Bind the listener (`:0` is fine) and start serving on the current runtime, returning a
    /// handle that reports the bound address and can be shut down gracefully (issue #342).
    pub async fn bind(self) -> anyhow::Result<RunningAdminApi> {
        // The third door onto this server, after the CLI and the C-ABI: an embedder building an
        // `AdminApiServer` directly (documented in docs/embedding/server.md). Without this check a
        // blank key here is caught only by `api_key_matches` failing closed, which is safe but
        // silent — the embedder would get a server that 401s everything while the log below claims
        // authentication is enabled. Checking at the one point all three doors converge on makes
        // the diagnosis loud wherever the key came from (issue #844).
        validate_admin_api_key(self.api_key.as_deref().map(String::as_str))?;
        // Issue #863: same convergence point, one step further — an off-host bind with no key at
        // all. Runs before `TcpListener::bind` so a `Refuse` policy never has to unwind a live
        // listener. Skipped when an outer door (CLI, C-ABI) already checked, so a single startup
        // warns once.
        if let Some(policy) = self.exposure {
            check_admin_exposure(
                self.addr,
                self.api_key.as_deref().map(String::as_str),
                policy,
            )?;
        }
        let listener = TcpListener::bind(self.addr).await?;
        let local_addr = listener.local_addr()?;
        info!(
            "Rift Admin API (Mountebank-compatible) listening on http://{}",
            local_addr
        );

        // Stated either way (issue #863): logging only the authenticated case made the riskier
        // posture the silent one, so the startup output never said the admin plane was open.
        if self.api_key.is_some() {
            info!("Admin API authentication enabled (--apikey)");
        } else {
            info!("Admin API authentication disabled — no --api-key is set");
        }

        let cancel = CancellationToken::new();
        let tracker = TaskTracker::new();
        let (loop_cancel, loop_tracker) = (cancel.clone(), tracker.clone());
        // The accept loop's outcome is published to a slot rather than left solely in the
        // JoinHandle, so `wait(&self)` can observe the exit without consuming the handle that
        // `join(self)`/`shutdown(&self)` need (issue #806).
        let done = CancellationToken::new();
        let outcome: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
        let (task_done, task_outcome) = (done.clone(), Arc::clone(&outcome));
        let release_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            // Releasing waiters from a drop guard rather than the tail of this block covers every
            // way the loop can end — normal return, panic unwind, and `shutdown`'s abort. Firing
            // only on the normal path would leave `wait()` blocked forever on a panicking accept
            // loop: precisely the death it exists to report (issue #806).
            let _release = ReleaseWaiters {
                done: task_done,
                outcome: Arc::clone(&task_outcome),
                shutdown_requested: release_cancel,
            };
            let result = accept_loop(
                listener,
                self.manager,
                self.api_key,
                self.config_source,
                self.allow_injection,
                self.intercept,
                self.scripts_dir,
                self.authorizer,
                crate::admin_api::handlers::system::ConfigSnapshot {
                    admin_port: local_addr.port(),
                    local_only: self.local_only,
                },
                loop_cancel,
                loop_tracker,
            )
            .await;
            // Log an accept-loop failure so it is observable even for an embedder that holds
            // the handle and never calls join() (join() still returns it for run()/RunningServer).
            if let Err(ref e) = result {
                tracing::error!("Admin API server error: {e:#}");
            }
            // Published before `_release` drops, so every released waiter sees the outcome.
            *task_outcome
                .lock()
                .expect("admin API outcome mutex poisoned") = Some(result);
        });

        Ok(RunningAdminApi {
            local_addr,
            cancel,
            tracker,
            task: Mutex::new(Some(task)),
            done,
            outcome,
        })
    }

    /// Run the admin API server until the accept loop exits. Delegates to `bind` + `join`
    /// so the binary path is byte-identical to binding then serving forever.
    pub async fn run(self) -> Result<(), anyhow::Error> {
        self.bind().await?.join().await
    }
}

/// Releases `RunningAdminApi::wait` callers however the accept-loop task ends (issue #806).
///
/// A tail-of-the-block `cancel()` would be skipped by a panic unwind or a `shutdown` abort,
/// stranding every waiter. Running it from `Drop` also lets an *unexpected* death be reported as
/// an error instead of a silent `Ok`, which is the whole point of `wait` for an embedder.
struct ReleaseWaiters {
    done: CancellationToken,
    outcome: Arc<Mutex<Option<anyhow::Result<()>>>>,
    /// The server's own shutdown token: when it is set, the task ending is expected, so no
    /// synthetic error is published.
    shutdown_requested: CancellationToken,
}

impl Drop for ReleaseWaiters {
    fn drop(&mut self) {
        // Recover from a poisoned lock rather than panicking: this runs during unwind, where a
        // second panic would abort the process.
        let mut slot = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() && !self.shutdown_requested.is_cancelled() {
            *slot = Some(Err(anyhow::anyhow!(
                "admin API accept loop terminated unexpectedly"
            )));
        }
        drop(slot);
        self.done.cancel();
    }
}

/// A bound, running admin API server (issue #342). Reports its listening address and offers a
/// graceful shutdown that does not require dropping the runtime.
pub struct RunningAdminApi {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Fired once the accept loop has exited and its outcome is published — the signal
    /// `wait(&self)` observes without touching `task` (issue #806).
    done: CancellationToken,
    /// The accept loop's result, delivered exactly once: the first caller of `wait`/`join` takes
    /// it, later callers get `Ok(())`. `anyhow::Error` is not `Clone`, so it cannot be shared.
    outcome: Arc<Mutex<Option<anyhow::Result<()>>>>,
}

impl RunningAdminApi {
    /// Build a `RunningAdminApi` whose "accept loop" is an arbitrary future — the seam for testing
    /// the failure path (issue #825).
    ///
    /// The real [`bind`](AdminApiServer::bind) can only fail its accept loop by genuinely breaking
    /// the listener, so the exactly-once error delivery of [`wait`](Self::wait) / [`join`](Self::join)
    /// — and the [`ReleaseWaiters`] drop guard behind it — is otherwise unreachable from outside this
    /// crate. An embedder that propagates a `RunningServer` outcome to process exit (rift-enterprise
    /// #42) needs to prove it reacts correctly, so pass a future that returns `Err`, or panics, and
    /// assert what your code does with it.
    ///
    /// No listener is bound: `local_addr()` reports `127.0.0.1:0` and the tracker is empty. Gated
    /// behind the `test-util` feature — it is test scaffolding, not a production constructor.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_accept_task<F>(loop_body: F) -> Self
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let done = CancellationToken::new();
        let outcome: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
        let (task_done, task_outcome) = (done.clone(), Arc::clone(&outcome));
        let release_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let _release = ReleaseWaiters {
                done: task_done,
                outcome: Arc::clone(&task_outcome),
                shutdown_requested: release_cancel,
            };
            let result = loop_body.await;
            *task_outcome
                .lock()
                .expect("admin API outcome mutex poisoned") = Some(result);
        });

        Self {
            local_addr: "127.0.0.1:0".parse().expect("test addr"),
            cancel,
            tracker: TaskTracker::new(),
            task: Mutex::new(Some(task)),
            done,
            outcome,
        }
    }
    /// The actual bound address (a `:0` request resolves to the OS-assigned port here).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting new connections, give in-flight connections a bounded grace, then return.
    /// Idempotent: a second call is a no-op.
    pub async fn shutdown(&self) {
        // Signals both the accept loop (stop accepting) and each live connection (which then
        // performs a hyper graceful shutdown).
        self.cancel.cancel();

        if let Some(task) = take_task(&self.task) {
            let abort = task.abort_handle();
            if tokio::time::timeout(SHUTDOWN_GRACE, task).await.is_err() {
                abort.abort();
            }
        }

        // Wait for in-flight connections to finish within the grace bound. They observe the
        // cancellation above and drain; the timeout bounds a pathologically slow one.
        self.tracker.close();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.tracker.wait())
            .await
            .is_err()
        {
            debug!(
                "Admin API shutdown: in-flight connections did not drain within the grace period"
            );
        }

        // Release any `wait(&self)` unconditionally. The abort above can kill the task before it
        // publishes its outcome, which would otherwise strand every waiter forever (issue #806).
        self.done.cancel();
    }

    /// Run until the accept loop exits (returns immediately if already shut down).
    ///
    /// Shares the exactly-once error delivery described on [`wait`](Self::wait): if a `wait` caller
    /// already took the accept loop's error, `join` returns `Ok(())`.
    pub async fn join(self) -> anyhow::Result<()> {
        match take_task(&self.task) {
            Some(task) => match task.await {
                Ok(()) => self.take_outcome(),
                Err(join_err) => Err(anyhow::anyhow!("admin API task failed: {join_err}")),
            },
            None => self.take_outcome(),
        }
    }

    /// Wait for the accept loop to exit **without consuming the handle** — so a caller can race
    /// "serve until the admin plane dies" against its own shutdown signal and still call
    /// [`shutdown`](Self::shutdown) afterwards (issue #806).
    ///
    /// The accept loop's error is delivered to the **first** caller only; subsequent calls (and
    /// calls after a `shutdown` that aborted the task) return `Ok(())`.
    pub async fn wait(&self) -> anyhow::Result<()> {
        self.done.cancelled().await;
        self.take_outcome()
    }

    /// Take the published accept-loop result. `Ok(())` when it was already taken or when the task
    /// was aborted before publishing.
    fn take_outcome(&self) -> anyhow::Result<()> {
        self.outcome
            .lock()
            .expect("admin API outcome mutex poisoned")
            .take()
            .unwrap_or(Ok(()))
    }
}

fn take_task<T>(slot: &Mutex<Option<JoinHandle<T>>>) -> Option<JoinHandle<T>> {
    slot.lock().expect("admin API task mutex poisoned").take()
}

/// Accept connections until `cancel` fires or the listener errors. Each connection is tracked
/// so `shutdown` can wait for in-flight requests to drain.
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: TcpListener,
    manager: Arc<ImposterManager>,
    api_key: Option<Arc<String>>,
    config_source: Option<ReloadSource>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
    authorizer: Option<Arc<dyn AdminAuthorizer>>,
    // The `GET /config` values that used to be hardcoded literals (issue #879).
    config_snapshot: crate::admin_api::handlers::system::ConfigSnapshot,
    cancel: CancellationToken,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    // Read HTTP tuning once per listener, not per accepted connection (issue #716).
    let http_tuning = rift_mock_core::proxy::HttpTuning::from_env();
    // `None` (the default) preserves today's behavior exactly: no semaphore, no permit.
    let connection_semaphore = http_tuning
        .max_connections
        .map(|n| Arc::new(tokio::sync::Semaphore::new(n)));

    // Accept-error handling, identical to the data plane's (issue #750, now shared from
    // `proxy::network`). Previously a single `accept()` failure propagated out of this loop and
    // ended the admin server — and an embedder that races `RunningServer::wait()` turns that into
    // a whole node leaving the cluster, so one transient ECONNABORTED or a momentary EMFILE became
    // a fleet-wide correlated restart (issue #826).
    let mut backoff = AcceptBackoff::new();
    let mut error_log = AcceptErrorLog::default();
    // Clears its contribution on every exit path, including a cancel break or a panic (#838).
    let mut outage = rift_mock_core::extensions::AcceptOutageGuard::new("admin");
    // Resolved once per loop, not per error (#840).
    let accept_errors = rift_mock_core::extensions::AcceptErrorCounters::new("admin");

    loop {
        // Acquire a permit *before* accepting so a cap holds connections back in the listener
        // backlog rather than accepting-then-failing. Raced against `cancel` so a saturated cap
        // never delays admin-server shutdown.
        let permit = match &connection_semaphore {
            Some(sem) => {
                let acquire = Arc::clone(sem).acquire_owned();
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    acquired = acquire => match acquired {
                        Ok(permit) => Some(permit),
                        Err(_) => break,
                    },
                }
            }
            None => None,
        };
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, _) = match accepted {
            Ok(accepted) => {
                // Recovery: only the transition out of a systemic-error state logs and resets the
                // backoff, so a healthy accept path pays one branch.
                if let Some(suppressed) = error_log.on_success() {
                    outage.exit();
                    info!(
                        suppressed,
                        "admin API accept loop recovered after {suppressed} suppressed error(s)"
                    );
                    backoff.reset();
                }
                accepted
            }
            // A broken listener fd is not a blip and cannot be cured by waiting: retrying it
            // forever would leave the process alive with a permanently dead control plane, and an
            // embedder racing `RunningServer::wait()` would never learn (issue #826). This fatal
            // class is deliberately **admin-only** — the shared #750 classifier stays two-way
            // because a dying imposter serve loop is independently recoverable through the
            // still-live admin API, whereas nothing outranks the admin plane.
            Err(e) if is_fatal_listener_error(&e) => {
                return Err(anyhow::anyhow!(
                    "admin API listener is unusable, giving up: {e}"
                ));
            }
            Err(e) => match classify_accept_error(&e) {
                // Expected under load — retry immediately, no backoff, no error spam.
                AcceptErrorClass::Transient => {
                    accept_errors.record_transient();
                    debug!("transient accept error on the admin listener: {e}");
                    continue;
                }
                // Systemic (fd exhaustion / unknown): log once on entry, then back off. Raced
                // against `cancel` so a backoff sleep never delays admin-server shutdown.
                AcceptErrorClass::Systemic => {
                    accept_errors.record_systemic();
                    match error_log.on_error() {
                        Some(AcceptErrorEvent::Onset) => {
                            outage.enter();
                            error!(
                                "accept error on the admin listener: {e}; backing off \
                                 (further errors suppressed until recovery)"
                            );
                        }
                        Some(AcceptErrorEvent::StillDown { suppressed }) => {
                            error!(
                                suppressed,
                                "admin listener still failing to accept after {suppressed} \
                                 suppressed error(s): {e}"
                            );
                        }
                        None => {}
                    }
                    let delay = backoff.next_delay();
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => break,
                    }
                    continue;
                }
            },
        };
        let io = TokioIo::new(stream);
        let manager = Arc::clone(&manager);
        let api_key = api_key.clone();
        let config_source = config_source.clone();
        let intercept = intercept.clone();
        let authorizer = authorizer.clone();
        let scripts_dir = scripts_dir.clone();
        let conn_cancel = cancel.clone();

        tracker.spawn(async move {
            // Held for the connection's lifetime; released to the semaphore when this task ends.
            let _permit = permit;
            let stream_cancel = conn_cancel.clone();
            let service = service_fn(move |req| {
                let manager = Arc::clone(&manager);
                let api_key = api_key.clone();
                let config_source = config_source.clone();
                let intercept = intercept.clone();
                let authorizer = authorizer.clone();
                let scripts_dir = scripts_dir.clone();
                let stream_cancel = stream_cancel.clone();
                async move {
                    // Per-request annotation scope + response decorator (issue #318):
                    // every response through this listener — including the `/__rift/`
                    // gateway — is decorated with phase `Admin`.
                    let decorator = manager.response_decorator();
                    let (result, annotations) = with_annotation_scope(async move {
                        // The single-port gateway (`/__rift/...`, issue #212) is data-plane
                        // imposter traffic, not the admin control plane — it mirrors direct
                        // per-imposter-port access and so is NOT gated by the admin `--apikey`
                        // (which would otherwise force app-under-test traffic to carry the admin
                        // key and would leak that Authorization header into imposter predicates).
                        let is_gateway = req.uri().path().starts_with("/__rift/");
                        if let Some(ref key) = api_key
                            && !is_gateway
                        {
                            let auth = req
                                .headers()
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            if !api_key_matches(auth, key.as_str()) {
                                return Ok::<_, hyper::Error>(box_full(unauthorized_response()));
                            }
                        }
                        // Authorization (issue #854), strictly after authentication. Ordering is
                        // load-bearing: moving the api-key gate below this would let an
                        // unauthenticated request to an unknown path answer 404 instead of 401,
                        // turning unknown-path responses into a route-existence oracle.
                        //
                        // Gateway traffic is exempt for the same reason it skips the api key —
                        // `classify` returns None for `/__rift/`, so app-under-test requests are
                        // never asked to carry an admin identity.
                        // Attribution for change events (issue #855). Stays `None` unless an
                        // authorizer both runs and names someone, so an embedder who installs no
                        // authorizer sees exactly the previous behaviour and data.
                        let mut principal: Option<String> = None;
                        if let Some(ref authorizer) = authorizer
                            && let Some(target) = authz::classify(req.method(), req.uri().path())
                        {
                            let params: Vec<(&str, &str)> = target
                                .params
                                .iter()
                                .map(|(name, value)| (*name, value.as_str()))
                                .collect();
                            let decision = authorizer.authorize(AuthzRequest {
                                credential: req
                                    .headers()
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok()),
                                action: target.action,
                                port: target.port,
                                space: target.space.as_deref(),
                                scope: req
                                    .headers()
                                    .get(SCOPE_HEADER)
                                    .and_then(|v| v.to_str().ok()),
                                params: &params,
                            });
                            match decision {
                                AuthzDecision::Allow { principal: allowed } => {
                                    principal = allowed;
                                }
                                AuthzDecision::Deny { reason } => {
                                    return Ok::<_, hyper::Error>(box_full(forbidden_response(
                                        reason,
                                    )));
                                }
                            }
                        }
                        // Admin SSE stream (issue #461): `/events` + the
                        // `/imposters/{port}/savedRequests/stream` alias. Runs AFTER the auth gate
                        // above, and BEFORE the `Full<Bytes>` router so the streaming body type never
                        // touches the router or its handlers.
                        if let Some(forced_port) = events::stream_target(req.uri().path()) {
                            return Ok::<_, hyper::Error>(events::handle_stream(
                                &manager,
                                req.uri().query(),
                                forced_port,
                                stream_cancel,
                            ));
                        }
                        // Only the router mutates, so the attribution scope wraps just this call —
                        // the SSE stream above is read-only and returns before it (issue #855).
                        with_principal_scope(
                            principal,
                            route_request(
                                req,
                                manager,
                                config_source,
                                allow_injection,
                                intercept,
                                scripts_dir,
                                config_snapshot,
                            ),
                        )
                        .await
                        .map(box_full)
                    })
                    .await;
                    let mut response = result?;
                    if let Some(decorator) = decorator {
                        decorator.decorate(
                            ResponsePhase::Admin,
                            None,
                            &annotations,
                            response.headers_mut(),
                        );
                    }
                    Ok::<_, hyper::Error>(response)
                }
            });

            // Both builders yield a Connection with the same drive/graceful-shutdown shape;
            // only the protocol negotiation differs (issue #378 force-disable escape hatch).
            macro_rules! drive_conn {
                ($conn:expr) => {{
                    let conn = $conn;
                    tokio::pin!(conn);
                    tokio::select! {
                        res = conn.as_mut() => {
                            if let Err(e) = res {
                                debug!("Admin API connection error: {}", e);
                            }
                        }
                        _ = conn_cancel.cancelled() => {
                            conn.as_mut().graceful_shutdown();
                            let _ = conn.await;
                        }
                    }
                }};
            }

            if rift_mock_core::util::http2_disabled() {
                let mut builder = hyper::server::conn::http1::Builder::new();
                // A timer is required for `header_read_timeout` to take effect (hyper panics on
                // serve_connection otherwise) — always paired with it (issue #716).
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(http_tuning.header_read_timeout)
                    .max_buf_size(http_tuning.max_buf_size);
                drive_conn!(builder.serve_connection(io, service));
            } else {
                let mut builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                // The h1 buffer/timeout knobs live on the `.http1()` sub-config of the auto builder.
                builder
                    .http1()
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(http_tuning.header_read_timeout)
                    .max_buf_size(http_tuning.max_buf_size);
                drive_conn!(builder.serve_connection(io, service));
            }
        });
    }
    Ok(())
}

/// Constant-time equality for the admin API key.
///
/// A plain `!=` short-circuits at the first differing byte, letting a network
/// attacker recover the key byte-by-byte from response-timing differences
/// (issue #548). `ConstantTimeEq` compares every byte regardless of where the
/// mismatch is; the length check it performs first is not secret.
fn api_key_matches(provided: &str, expected: &str) -> bool {
    // Fail closed on a blank configured key (issue #844). A request with no `authorization` header
    // reaches here as `""`, so comparing it against a blank key returns true and authenticates
    // everyone — on the configuration that most looks like it should fail closed. A classifier that
    // cannot tell callers apart must treat them as unauthenticated.
    //
    // This early return branches only on the SERVER's configured key, never on the caller-supplied
    // one, so it leaks nothing about the secret and leaves the constant-time property from #548
    // intact for the comparison that matters.
    if expected.trim().is_empty() {
        return false;
    }
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Reject a blank admin API key where it is configured (issue #844).
///
/// A blank key is a misconfiguration, not a key: `Some("")` switches the auth gate on, and a
/// request with no `Authorization` header also degrades to `""`, so the gate then matches everyone.
/// Rejecting rather than normalising to "no auth" is deliberate — a silent downgrade leaves the
/// operator believing a key is in force. Whitespace-only counts as blank; a key that merely
/// *contains* spaces is valid and is never trimmed for comparison.
///
/// See the CHANGELOG entry and `docs/configuration/cli.md` for the operator-facing account of how
/// a blank value gets configured in the first place.
pub fn validate_admin_api_key(api_key: Option<&str>) -> anyhow::Result<()> {
    if let Some(key) = api_key
        && key.trim().is_empty()
    {
        anyhow::bail!(
            "the admin API key (`--api-key` / `MB_APIKEY` / `apiKey`) is set to a blank value. \
             That would enable the auth gate and then accept every unauthenticated request, \
             leaving the admin API open while reporting as protected. Set a real token, or omit \
             it entirely to run the admin API explicitly unauthenticated."
        );
    }
    Ok(())
}

/// What to do when the admin plane would be reachable off-host with no API key (issue #863).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdminExposurePolicy {
    /// Log a warning and continue. The default, and deliberately so: `--host` defaults to
    /// `0.0.0.0` and containers need it (binding loopback inside Docker makes the published port
    /// unreachable), so refusing here would break the no-argument invocation, every quickstart and
    /// every CI sandbox. A refusal everyone opts out of immediately is worse than a warning read
    /// once — the opt-out ends up permanent in the base image.
    #[default]
    Warn,
    /// Refuse to start (`--require-admin-auth` / `RIFT_REQUIRE_ADMIN_AUTH`). Opting *in* to
    /// strictness rather than out of a refusal keeps the default compatible and needs no escape
    /// hatch of its own.
    Refuse,
}

impl From<bool> for AdminExposurePolicy {
    /// `true` is the `--require-admin-auth` / `requireAdminAuth` spelling of [`Refuse`]. One
    /// mapping, so the CLI, the C-ABI and the embedder builder cannot drift on what the flag means.
    ///
    /// [`Refuse`]: AdminExposurePolicy::Refuse
    fn from(require_admin_auth: bool) -> Self {
        if require_admin_auth {
            Self::Refuse
        } else {
            Self::Warn
        }
    }
}

/// Warn or refuse when the admin plane would bind a non-loopback address with no API key
/// (issue #863).
///
/// `addr` is the *resolved* bind address, so `0.0.0.0`, `::` and any specific interface are all
/// covered by one `is_loopback()` test — an unspecified address is not loopback, which is exactly
/// the case this catches. Rift parses `--host` as a literal `SocketAddr` and never resolves DNS, so
/// there is no name left to resolve by the time the address gets here.
///
/// Callers must invoke this before binding anything: under [`AdminExposurePolicy::Refuse`] the
/// error must not have to unwind a listener that is already up.
///
/// `api_key` being `Some(_)` is taken to mean a usable key, which holds because
/// [`validate_admin_api_key`] rejects a blank one upstream (issue #844) — reversing that order
/// would let `Some("")` satisfy the very gate it defeats.
pub fn check_admin_exposure(
    addr: SocketAddr,
    api_key: Option<&str>,
    policy: AdminExposurePolicy,
) -> anyhow::Result<()> {
    // `to_canonical` first: `Ipv6Addr::is_loopback` matches only the literal `::1`, so an
    // IPv4-mapped `::ffff:127.0.0.1` — which some address-normalising front ends hand over — would
    // otherwise be judged off-host and refused under `Refuse` despite being loopback-only. The
    // mapping is safe in the other direction too: `::ffff:10.0.0.5` canonicalises to `10.0.0.5`,
    // which is still not loopback, so nothing genuinely reachable becomes exempt.
    if api_key.is_some() || addr.ip().to_canonical().is_loopback() {
        return Ok(());
    }
    let message = format!(
        "the admin API is bound to {addr}, which is reachable from outside this host, with no API \
         key — anyone who can reach that address can create imposters and drive the TLS intercept \
         proxy. Set `--api-key <token>` (`MB_APIKEY`), or restrict the bind with `--local-only` or \
         `--host 127.0.0.1`. Set `--require-admin-auth` (`RIFT_REQUIRE_ADMIN_AUTH`) to make this a \
         startup failure instead of a warning."
    );
    match policy {
        AdminExposurePolicy::Warn => {
            warn!("{message}");
            Ok(())
        }
        AdminExposurePolicy::Refuse => anyhow::bail!(message),
    }
}

/// Box a `Full<Bytes>` response into the streaming-unified `AdminBody` (issue #461), so the normal
/// router path and the SSE stream path share one response type. `Full`'s error is `Infallible`, so
/// the `map_err` closure is unreachable.
fn box_full(resp: Response<Full<Bytes>>) -> Response<AdminBody> {
    resp.map(|body| body.map_err(|never| match never {}).boxed())
}

fn unauthorized_response() -> Response<Full<Bytes>> {
    let body = r#"{"errors":[{"code":"unauthorized","type":"unauthorized","message":"Invalid authorization token"}]}"#;
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("infallible")
}

/// An installed [`AdminAuthorizer`] denied an *authenticated* request (issue #854).
///
/// 403, not 401: the credential was accepted, so telling the caller to re-authenticate would send
/// them round a loop that cannot succeed. 401 stays reserved for a missing or malformed
/// credential.
///
/// `reason` comes from the authorizer and is echoed to the caller, which is why it is a
/// `&'static str` — an embedder has to write it as a literal rather than formatting a principal,
/// a policy id or an internal error into it by accident.
fn forbidden_response(reason: &'static str) -> Response<Full<Bytes>> {
    // The shared builder, not a hand-rolled envelope: it emits `code = "403"` and
    // `type = "insufficient access"` per the #797 format that all four SDKs and rift-conformance
    // parse. `unauthorized_response` above predates that format and is grandfathered; copying its
    // shape onto a brand-new surface would spread the wart rather than contain it.
    crate::admin_api::types::error_response(StatusCode::FORBIDDEN, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unauthorized_response_status() {
        let resp = unauthorized_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_unauthorized_response_body() {
        use http_body_util::BodyExt;
        let resp = unauthorized_response();
        let body_bytes = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resp.into_body().collect())
            .unwrap()
            .to_bytes();
        let body_str = std::str::from_utf8(&body_bytes).unwrap();
        let json: serde_json::Value = serde_json::from_str(body_str).unwrap();
        assert_eq!(json["errors"][0]["code"], "unauthorized");
        // Issue #797 invariant 3: on a door whose `code` is already a slug, `type` is that same
        // slug. Asserted here because this envelope is a hand-written literal, not built by
        // `error_body_typed` — nothing else would catch the two drifting apart.
        assert_eq!(json["errors"][0]["type"], "unauthorized");
        assert_eq!(
            json["errors"][0]["type"], json["errors"][0]["code"],
            "type and code must agree on a slug door"
        );
        assert!(!json["errors"][0]["message"].as_str().unwrap().is_empty());
    }

    #[test]
    fn test_admin_server_new_with_api_key() {
        let manager = Arc::new(ImposterManager::new());
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server = AdminApiServer::new(addr, manager, Some("secret".to_string()));
        assert!(server.api_key.is_some());
        assert_eq!(server.api_key.unwrap().as_str(), "secret");
    }

    #[test]
    fn test_admin_server_new_without_api_key() {
        let manager = Arc::new(ImposterManager::new());
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server = AdminApiServer::new(addr, manager, None);
        assert!(server.api_key.is_none());
    }

    #[test]
    fn api_key_matches_accepts_correct() {
        assert!(api_key_matches("s3cret-token", "s3cret-token"));
    }

    #[test]
    fn api_key_matches_rejects_wrong() {
        assert!(!api_key_matches("s3cret-tokeX", "s3cret-token"));
        // Differ in the first byte — a short-circuiting compare would return
        // fastest here; the constant-time compare must still reject it.
        assert!(!api_key_matches("Xs3cret-token", "s3cret-token"));
    }

    #[test]
    fn api_key_matches_rejects_wrong_length() {
        assert!(!api_key_matches("s3cret", "s3cret-token"));
        assert!(!api_key_matches("s3cret-token-extra", "s3cret-token"));
    }

    #[test]
    fn api_key_matches_rejects_empty_against_nonempty() {
        assert!(!api_key_matches("", "s3cret-token"));
    }

    // Issue #844: this used to assert the OPPOSITE — that two empty strings match — on the
    // reasoning that "no key configured is handled by the `Some(key)` guard at the call site".
    // `Some("")` is `Some`, so that guard does not fire: the gate switched ON and then
    // authenticated every anonymous request, because a missing `authorization` header also
    // degrades to `""`. A security classifier that cannot identify a caller must fail closed, so a
    // blank configured key now matches nothing. `validate_admin_api_key` rejects that config long
    // before a request arrives; this is the backstop for an entry point that forgets to call it.
    #[test]
    fn api_key_matches_fails_closed_on_a_blank_expected_key() {
        assert!(
            !api_key_matches("", ""),
            "a blank configured key must not authenticate an anonymous request"
        );
        assert!(!api_key_matches("anything", ""));
        assert!(!api_key_matches("", "   "));
        assert!(
            !api_key_matches("   ", "   "),
            "a whitespace-only key is blank too, and must not authenticate its own echo"
        );
    }

    // Issue #844 AC1/AC2: the shared validator both config boundaries use. A blank key is a
    // misconfiguration to reject loudly, never a key and never a silent downgrade to "no auth" —
    // downgrading would leave the operator believing a key is in force.
    #[test]
    fn blank_api_key_is_rejected() {
        // `\u{00a0}` (non-breaking space) is included deliberately: `str::trim` uses
        // `char::is_whitespace`, so it counts as blank — and a non-breaking space is exactly the
        // kind of thing that survives a copy-paste out of a rendered config or a wiki page.
        for blank in ["", " ", "   ", "\t", "\n", " \t\n ", "\u{00a0}"] {
            let err = validate_admin_api_key(Some(blank))
                .expect_err("a blank api key must be rejected")
                .to_string();
            assert!(
                err.contains("--api-key"),
                "the error must name the flag an operator would fix, got: {err}"
            );
        }
    }

    // Issue #844 AC4: the fix must not change either working configuration — a real key, or no key
    // at all (explicitly unauthenticated).
    #[test]
    fn a_real_or_absent_api_key_is_accepted() {
        assert!(validate_admin_api_key(Some("s3cret-token")).is_ok());
        assert!(
            validate_admin_api_key(Some(" padded ")).is_ok(),
            "a key with surrounding spaces is unusual but not blank; it stays a valid key"
        );
        assert!(
            validate_admin_api_key(None).is_ok(),
            "no key at all remains a supported, explicitly unauthenticated configuration"
        );
    }

    // Issue #844: a key that is not blank keeps matching exactly as before, including the
    // surrounding-whitespace case — the validator allows it, so the comparison must too, byte for
    // byte and without trimming.
    #[test]
    fn a_padded_key_still_matches_byte_for_byte() {
        assert!(api_key_matches(" padded ", " padded "));
        assert!(
            !api_key_matches("padded", " padded "),
            "the comparison must not trim; a trimmed guess is a different key"
        );
    }

    // ── issue #863: the keyless off-host admin plane ─────────────────────────────────────────
    //
    // The classifier is one `is_loopback()` test because the bind address reaching it is already
    // resolved and parsed as a literal `SocketAddr` — Rift never resolves DNS for `--host`, so
    // there is no name to look up here and no case where the "address" is still a hostname.

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("test address parses")
    }

    /// Every address the check must treat as off-host. `0.0.0.0` and `::` are *unspecified*, not
    /// loopback: binding them reaches every interface, which is exactly the case being caught.
    const OFF_HOST: [&str; 4] = [
        "0.0.0.0:2525",
        "[::]:2525",
        "10.0.0.5:2525",
        // Canonicalising must not exempt a genuinely reachable address: this maps to `10.0.0.5`,
        // which is still not loopback.
        "[::ffff:10.0.0.5]:2525",
    ];
    /// Loopback in both families. `::1` is the one an IPv4-only `is_loopback` would misclassify;
    /// `::ffff:127.0.0.1` is the one `Ipv6Addr::is_loopback` alone misclassifies, since it matches
    /// only the literal `::1` — hence the `to_canonical()` in the classifier.
    const LOOPBACK: [&str; 3] = ["127.0.0.1:2525", "[::1]:2525", "[::ffff:127.0.0.1]:2525"];

    // AC1: `Refuse` errors on exactly the off-host × no-key cells and nowhere else.
    #[test]
    fn refuse_rejects_exactly_the_keyless_off_host_binds() {
        for a in OFF_HOST {
            assert!(
                check_admin_exposure(addr(a), None, AdminExposurePolicy::Refuse).is_err(),
                "{a} with no key must be refused under --require-admin-auth"
            );
        }
        for a in LOOPBACK {
            assert!(
                check_admin_exposure(addr(a), None, AdminExposurePolicy::Refuse).is_ok(),
                "{a} is loopback: unreachable off-host, so no key is needed"
            );
        }
    }

    // AC2: IPv6 loopback is loopback. Called out separately from the loop above because getting
    // this wrong is silent — `::1` would be refused as if it were world-reachable.
    #[test]
    fn ipv6_loopback_is_not_treated_as_exposed() {
        assert!(
            check_admin_exposure(addr("[::1]:2525"), None, AdminExposurePolicy::Refuse).is_ok()
        );
    }

    // AC3: the flag gates on authentication, not on the address. A real key makes any bind
    // acceptable — otherwise `--require-admin-auth` would just be a second `--local-only`.
    #[test]
    fn a_real_key_satisfies_the_check_on_any_address() {
        for a in OFF_HOST.iter().chain(LOOPBACK.iter()) {
            assert!(
                check_admin_exposure(addr(a), Some("s3cr3t"), AdminExposurePolicy::Refuse).is_ok(),
                "{a} with a real key is authenticated, so it must be accepted"
            );
        }
    }

    // The default posture is advisory: the same cell that errors under `Refuse` must return `Ok`
    // under `Warn`. This is the compatibility guarantee for every existing keyless deployment.
    #[test]
    fn warn_never_refuses() {
        for a in OFF_HOST {
            assert!(
                check_admin_exposure(addr(a), None, AdminExposurePolicy::Warn).is_ok(),
                "{a} must only warn by default; refusing is opt-in"
            );
        }
        assert_eq!(
            AdminExposurePolicy::default(),
            AdminExposurePolicy::Warn,
            "the default policy must be Warn, so an embedder that sets nothing is unaffected"
        );
    }

    // AC4: an operator must be able to act on the message without reading source, so it names the
    // address and every remedy — including `--require-admin-auth` itself, for anyone who arrives
    // via the warning and wants it to be fatal next time.
    #[test]
    fn the_refusal_names_the_address_and_every_remedy() {
        let err = check_admin_exposure(addr("0.0.0.0:2525"), None, AdminExposurePolicy::Refuse)
            .expect_err("a keyless 0.0.0.0 bind must be refused under Refuse")
            .to_string();
        for expected in [
            "0.0.0.0:2525",
            "--api-key",
            "--local-only",
            "--host 127.0.0.1",
            "--require-admin-auth",
        ] {
            assert!(
                err.contains(expected),
                "the refusal must mention `{expected}`, got: {err}"
            );
        }
    }

    // Interaction with #844: a blank key is rejected by `validate_admin_api_key` upstream, so it
    // can never reach this check. Were the order ever reversed, `Some("")` would read as "a key is
    // set" here and silently satisfy the very gate it defeats — pin the ordering assumption.
    #[test]
    fn a_blank_key_is_already_rejected_before_this_check_runs() {
        assert!(
            validate_admin_api_key(Some("")).is_err(),
            "the blank-key validator must run first; this check trusts that Some(_) is a real key"
        );
    }

    // AC12 — the third door. The CLI and the C-ABI both run the check themselves and then call
    // `with_exposure_checked()`, so `bind()`'s own check is reached ONLY by an embedder building an
    // `AdminApiServer` directly. Without these two tests `with_require_admin_auth` and the check
    // inside `bind()` are public API that no test exercises: both could be deleted and everything
    // would still pass.

    #[tokio::test]
    async fn with_require_admin_auth_refuses_at_the_embedder_door_before_binding() {
        // `expect_err` is unavailable here — `RunningAdminApi` is not `Debug` — and matching also
        // lets the success arm shut the listener down rather than leaking it into the test run.
        let err =
            match AdminApiServer::new(addr("0.0.0.0:0"), Arc::new(ImposterManager::new()), None)
                .with_require_admin_auth(true)
                .bind()
                .await
            {
                Ok(running) => {
                    running.shutdown().await;
                    panic!(
                        "an embedder opting into strict mode must not get a keyless off-host bind"
                    );
                }
                Err(err) => err,
            };
        // Port 0 would bind successfully if the check never ran, so an exposure message here — as
        // opposed to an I/O error — is what proves `bind()` refused rather than failed to listen.
        assert!(
            err.to_string().contains("--api-key"),
            "the embedder must get the exposure refusal, not an I/O error: {err}"
        );
    }

    #[tokio::test]
    async fn the_embedder_door_defaults_to_warning_and_still_binds() {
        let running =
            AdminApiServer::new(addr("0.0.0.0:0"), Arc::new(ImposterManager::new()), None)
                .bind()
                .await
                .expect(
                    "the default policy warns; it must never refuse an embedder's keyless bind",
                );
        running.shutdown().await;
    }
}

/// Issue #806: white-box tests for the `wait` seam's failure interleavings. These construct a
/// `RunningAdminApi` around a stand-in task rather than a real accept loop, because the paths that
/// matter — an abort before the outcome is published, a panicking loop, and the exactly-once error
/// hand-off — cannot be provoked through the public bind/shutdown API (a healthy loop always exits
/// well inside the shutdown grace).
#[cfg(test)]
mod wait_seam_tests {
    use super::*;

    /// A `RunningAdminApi` whose "accept loop" is `task`, wired to the same release guard the real
    /// one uses so the drop-path behaviour under test is the shipped behaviour.
    fn running_with_task<F>(loop_body: F) -> RunningAdminApi
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        // Delegates to the public seam so the tested construction and the one embedders get are
        // the same code (issue #825).
        RunningAdminApi::with_accept_task(loop_body)
    }

    /// `shutdown` aborts a loop that outlives the grace window, so the task never publishes an
    /// outcome. Waiters must still be released — this is the interleaving the release guard and
    /// `shutdown`'s unconditional cancel both exist to cover.
    #[tokio::test]
    async fn wait_is_released_when_shutdown_aborts_an_unresponsive_loop() {
        let running = running_with_task(async {
            std::future::pending::<()>().await;
            Ok(())
        });

        tokio::time::timeout(Duration::from_secs(5), running.shutdown())
            .await
            .expect("shutdown gives up on the wedged loop within its grace bound");

        let waited = tokio::time::timeout(Duration::from_secs(2), running.wait())
            .await
            .expect("an aborted loop must not strand waiters");
        assert!(
            waited.is_ok(),
            "an abort during a requested shutdown is not an error"
        );
    }

    /// A panicking accept loop is the death `wait` exists to report. It must resolve — and as an
    /// error, not a silent `Ok`, since no shutdown was requested.
    #[tokio::test]
    async fn wait_reports_a_panicking_loop_instead_of_hanging() {
        let running = running_with_task(async { panic!("accept loop exploded") });

        let waited = tokio::time::timeout(Duration::from_secs(2), running.wait())
            .await
            .expect("a panicking loop must release waiters");
        let err = waited.expect_err("an unrequested death is an error");
        assert!(
            err.to_string().contains("terminated unexpectedly"),
            "error should name the unexpected termination, got: {err}"
        );
    }

    /// The documented exactly-once contract: the accept loop's error goes to the first caller, and
    /// later callers get `Ok(())` rather than a duplicate or a hang.
    #[tokio::test]
    async fn accept_loop_error_is_delivered_exactly_once() {
        let running = running_with_task(async { Err(anyhow::anyhow!("listener died")) });

        let first = tokio::time::timeout(Duration::from_secs(2), running.wait())
            .await
            .expect("wait resolves once the loop has returned");
        let err = first.expect_err("the first caller receives the accept loop's error");
        assert!(
            err.to_string().contains("listener died"),
            "the real error must survive, got: {err}"
        );

        let second = tokio::time::timeout(Duration::from_secs(2), running.wait())
            .await
            .expect("a second wait returns immediately");
        assert!(
            second.is_ok(),
            "the error is delivered once; later callers get Ok"
        );
    }
}

// Issue #826: pins which accept errors must NOT end the admin server (the errnos the issue names)
// and which still must. The visibility of the shared #750 machinery is already compile-gated by
// `accept_loop` itself using it; these assertions cover the classification decisions.
#[cfg(test)]
mod admin_accept_error_tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn admin_accept_error_classification() {
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::Interrupted,
            ErrorKind::ConnectionReset,
        ] {
            assert_eq!(
                classify_accept_error(&Error::from(kind)),
                AcceptErrorClass::Transient,
                "{kind:?} must retry immediately, never end the admin server"
            );
        }
        // EMFILE/ENFILE are the fd-exhaustion cases from the issue: back off, never terminate.
        for raw in [24, 23] {
            assert_eq!(
                classify_accept_error(&Error::from_raw_os_error(raw)),
                AcceptErrorClass::Systemic,
                "errno {raw} must back off, not terminate"
            );
        }
    }

    #[test]
    fn admin_backoff_and_error_log_are_usable_cross_crate() {
        let mut b = AcceptBackoff::new();
        assert_eq!(b.next_delay(), Duration::from_millis(1));
        assert_eq!(b.next_delay(), Duration::from_millis(2));
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(1), "reset re-arms");

        let mut log = AcceptErrorLog::default();
        assert_eq!(
            log.on_error(),
            Some(AcceptErrorEvent::Onset),
            "first systemic error logs once"
        );
        assert_eq!(log.on_error(), None, "subsequent errors are suppressed");
        assert_eq!(
            log.on_success(),
            Some(1),
            "recovery reports the suppressed count"
        );
        assert_eq!(log.on_success(), None, "steady state is silent");
    }
}
