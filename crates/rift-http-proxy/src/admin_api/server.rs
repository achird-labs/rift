//! Admin API server.

use crate::admin_api::handlers::events::{self, AdminBody};
use crate::admin_api::router::route_request;
use crate::config_loader::ConfigSource;
use crate::extensions::decorate::{ResponsePhase, with_annotation_scope};
use crate::imposter::ImposterManager;
use crate::intercept_control::InterceptControl;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use rift_mock_core::proxy::{
    AcceptBackoff, AcceptErrorClass, AcceptErrorLog, classify_accept_error,
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
use tracing::{debug, error, info};

/// Bounded grace given to in-flight connections on `shutdown()` before the wait is abandoned.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Admin API server for Rift
pub struct AdminApiServer {
    addr: SocketAddr,
    manager: Arc<ImposterManager>,
    api_key: Option<Arc<String>>,
    config_source: Option<Arc<ConfigSource>>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
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
        }
    }

    /// Set the config source (`--configfile`/`--datadir`) so `POST /admin/reload` can re-read it
    /// (issue #197). Without it, reload is a no-op.
    #[must_use]
    pub fn with_config_source(mut self, source: ConfigSource) -> Self {
        self.config_source = Some(Arc::new(source));
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
        let listener = TcpListener::bind(self.addr).await?;
        let local_addr = listener.local_addr()?;
        info!(
            "Rift Admin API (Mountebank-compatible) listening on http://{}",
            local_addr
        );

        if self.api_key.is_some() {
            info!("Admin API authentication enabled (--apikey)");
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

/// Whether an `accept(2)` error means the listener itself is unusable, as opposed to a transient
/// or resource-pressure condition that retrying can clear.
///
/// `EBADF` / `ENOTSOCK` / `EINVAL` say the descriptor is not a working listening socket; no amount
/// of backoff fixes that, so the admin accept loop terminates and lets
/// [`RunningAdminApi::wait`] report the death (issue #826). Everything else — including unknown
/// errnos — is left to the shared two-way classifier, which retries.
///
/// Unix-only by construction: these are POSIX errnos. On other platforms nothing is treated as
/// fatal, matching the conservative "retry rather than die" default.
#[cfg(unix)]
fn is_fatal_listener_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EBADF) | Some(libc::ENOTSOCK) | Some(libc::EINVAL)
    )
}

#[cfg(not(unix))]
fn is_fatal_listener_error(_e: &std::io::Error) -> bool {
    false
}

/// Accept connections until `cancel` fires or the listener errors. Each connection is tracked
/// so `shutdown` can wait for in-flight requests to drain.
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: TcpListener,
    manager: Arc<ImposterManager>,
    api_key: Option<Arc<String>>,
    config_source: Option<Arc<ConfigSource>>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
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
                    debug!("transient accept error on the admin listener: {e}");
                    continue;
                }
                // Systemic (fd exhaustion / unknown): log once on entry, then back off. Raced
                // against `cancel` so a backoff sleep never delays admin-server shutdown.
                AcceptErrorClass::Systemic => {
                    if error_log.on_error() {
                        error!(
                            "accept error on the admin listener: {e}; backing off \
                             (further errors suppressed until recovery)"
                        );
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
                        route_request(
                            req,
                            manager,
                            config_source,
                            allow_injection,
                            intercept,
                            scripts_dir,
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
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
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
        // Two empty strings are trivially equal — no key configured is handled
        // by the `Some(key)` guard at the call site, not here.
        assert!(api_key_matches("", ""));
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

        RunningAdminApi {
            local_addr: "127.0.0.1:0".parse().expect("test addr"),
            cancel,
            tracker: TaskTracker::new(),
            task: Mutex::new(Some(task)),
            done,
            outcome,
        }
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
        assert!(log.on_error(), "first systemic error logs once");
        assert!(!log.on_error(), "subsequent errors are suppressed");
        assert_eq!(
            log.on_success(),
            Some(1),
            "recovery reports the suppressed count"
        );
        assert_eq!(log.on_success(), None, "steady state is silent");
    }

    // The third class the issue asks for ("terminate only on genuinely fatal errors"): a broken
    // listener fd must still end the loop so `RunningAdminApi::wait()` reports a dead admin plane.
    #[cfg(unix)]
    #[test]
    fn broken_listener_errnos_are_fatal_but_pressure_is_not() {
        for raw in [libc::EBADF, libc::ENOTSOCK, libc::EINVAL] {
            assert!(
                is_fatal_listener_error(&Error::from_raw_os_error(raw)),
                "errno {raw} means the listener is unusable; retrying forever would hide a dead admin plane"
            );
        }
        // Recoverable pressure and transient blips must never be treated as fatal.
        for raw in [libc::EMFILE, libc::ENFILE, libc::ECONNABORTED, libc::EINTR] {
            assert!(
                !is_fatal_listener_error(&Error::from_raw_os_error(raw)),
                "errno {raw} is recoverable and must be retried, not fatal"
            );
        }
    }
}
