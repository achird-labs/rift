//! The front-door listener itself (issue #19 / U-11): one bound port, resolved per-request
//! against a hot-swappable [`CompiledRoutes`], falling back to the single-port gateway, falling
//! back to a 404 that says so.
//!
//! Structurally this is [`crate::server::bind_metrics_server`] +
//! `metrics_accept_loop` — same `TcpListener::bind` → `CancellationToken` + `TaskTracker` →
//! `tokio::spawn` shape, same shared accept-error/backoff machinery from
//! `rift_mock_core::proxy` and `rift_mock_core::extensions` — with the metrics-only service body
//! replaced by route resolution. Copied rather than shared because the two loops' *service*
//! bodies have nothing in common; the accept-loop plumbing around them is what is common, and it
//! is small enough that a shared abstraction would cost more to read than the duplication does.

use crate::front_door::route_table::{CompiledRoutes, Route};
use crate::imposter::ImposterManager;
use crate::response::{ErrorKind, error_response_typed};
use arc_swap::ArcSwap;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info};

/// Bounded grace given to in-flight connections on `shutdown()` (mirrors the metrics/admin
/// listeners' constant of the same name and value).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// The marker distinguishing "no route matched" from "a route matched but its imposter is gone"
/// — both are 404s, but only the former is a routing-table problem.
const NO_ROUTE_HEADER: &str = "x-rift-front-door";

/// Bind the front-door listener (`:0` is fine) and start serving, returning a handle that reports
/// the bound address and can be shut down gracefully. `routes` is read fresh on every request via
/// [`ArcSwap::load_full`], so an admin-API route-table update takes effect for the next request
/// with no listener restart.
pub async fn bind_front_door(
    addr: SocketAddr,
    manager: Arc<ImposterManager>,
    routes: Arc<ArcSwap<CompiledRoutes>>,
) -> anyhow::Result<RunningFrontDoor> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!("Front door listening on http://{}", local_addr);

    let cancel = CancellationToken::new();
    let tracker = TaskTracker::new();
    let (loop_cancel, loop_tracker) = (cancel.clone(), tracker.clone());
    let task = tokio::spawn(async move {
        let result =
            front_door_accept_loop(listener, manager, routes, loop_cancel, loop_tracker).await;
        // Preserve the metrics-listener precedent: an accept-loop failure is logged here because
        // nothing else joins this task by default.
        if let Err(ref e) = result {
            error!("Front door server error: {e:#}");
        }
        result
    });

    Ok(RunningFrontDoor {
        local_addr,
        cancel,
        tracker,
        task: Mutex::new(Some(task)),
    })
}

/// A bound, running front-door listener.
pub struct RunningFrontDoor {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

impl RunningFrontDoor {
    /// The actual bound address (a `:0` request resolves to the OS-assigned port here).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting new connections, give in-flight connections a bounded grace, then return.
    /// Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        // Bind the taken handle to a local so the MutexGuard drops before any `.await`.
        let task = self
            .task
            .lock()
            .expect("front door task mutex poisoned")
            .take();
        if let Some(task) = task {
            let abort = task.abort_handle();
            if tokio::time::timeout(SHUTDOWN_GRACE, task).await.is_err() {
                abort.abort();
            }
        }
        self.tracker.close();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.tracker.wait())
            .await
            .is_err()
        {
            debug!(
                "Front door shutdown: in-flight connections did not drain within the grace period"
            );
        }
    }

    /// Run until the accept loop exits (returns immediately if already shut down).
    pub async fn join(self) -> anyhow::Result<()> {
        let task = self
            .task
            .lock()
            .expect("front door task mutex poisoned")
            .take();
        match task {
            Some(task) => match task.await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("front door task failed: {join_err}")),
            },
            None => Ok(()),
        }
    }
}

async fn front_door_accept_loop(
    listener: TcpListener,
    manager: Arc<ImposterManager>,
    routes: Arc<ArcSwap<CompiledRoutes>>,
    cancel: CancellationToken,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rift_mock_core::proxy::{
        AcceptBackoff, AcceptErrorClass, AcceptErrorEvent, AcceptErrorLog, HttpTuning,
        classify_accept_error, is_fatal_listener_error,
    };

    // Read HTTP tuning once per listener, not per accepted connection.
    let http_tuning = HttpTuning::from_env();
    // `None` (the default) preserves today's behavior exactly: no semaphore, no permit, accept as
    // fast as the kernel hands connections over (issue #716).
    let connection_semaphore = http_tuning
        .max_connections
        .map(|n| std::sync::Arc::new(tokio::sync::Semaphore::new(n)));

    // Accept-error handling, identical to the metrics/admin loops' (issues #826, #834).
    let mut backoff = AcceptBackoff::new();
    let mut error_log = AcceptErrorLog::default();
    // Clears its contribution on every exit path, including a cancel break or a panic (#838).
    let mut outage = rift_mock_core::extensions::AcceptOutageGuard::new("front-door");
    // Resolved once per loop, not per error (#840).
    let accept_errors = rift_mock_core::extensions::AcceptErrorCounters::new("front-door");

    loop {
        // Acquire a permit *before* accepting so a cap holds connections back in the listener
        // backlog/kernel SYN queue rather than accepting them and then failing downstream. Raced
        // against `cancel` (like the accept below) so a saturated cap never delays shutdown.
        let permit = match &connection_semaphore {
            Some(sem) => {
                let acquire = sem.clone().acquire_owned();
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    acquired = acquire => match acquired {
                        Ok(permit) => Some(permit),
                        // Only closes on Drop, which never happens here — but if it ever does,
                        // stop accepting rather than panic.
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
                        "front door accept loop recovered after {suppressed} suppressed error(s)"
                    );
                    backoff.reset();
                }
                accepted
            }
            // A broken listener fd cannot be cured by waiting; end the loop so the failure reaches
            // the `Front door server error` log rather than spinning forever (issue #834).
            Err(e) if is_fatal_listener_error(&e) => {
                return Err(anyhow::anyhow!(
                    "front door listener is unusable, giving up: {e}"
                ));
            }
            Err(e) => match classify_accept_error(&e) {
                AcceptErrorClass::Transient => {
                    accept_errors.record_transient();
                    debug!("transient accept error on the front door listener: {e}");
                    continue;
                }
                AcceptErrorClass::Systemic => {
                    accept_errors.record_systemic();
                    match error_log.on_error() {
                        Some(AcceptErrorEvent::Onset) => {
                            outage.enter();
                            error!(
                                "accept error on the front door listener: {e}; backing off \
                                 (further errors suppressed until recovery)"
                            );
                        }
                        Some(AcceptErrorEvent::StillDown { suppressed }) => {
                            error!(
                                suppressed,
                                "front door listener still failing to accept after {suppressed} \
                                 suppressed error(s): {e}"
                            );
                        }
                        None => {}
                    }
                    // Raced against `cancel` so a backoff sleep never delays shutdown.
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
        let conn_cancel = cancel.clone();
        let manager = Arc::clone(&manager);
        let routes = Arc::clone(&routes);

        tracker.spawn(async move {
            // Held for the connection's lifetime; released back to the semaphore when this task
            // ends (issue #716).
            let _permit = permit;
            let service = service_fn(move |req: Request<Incoming>| {
                let manager = Arc::clone(&manager);
                let routes = Arc::clone(&routes);
                async move { handle_front_door_request(req, manager, routes).await }
            });

            // Both builders yield a Connection with the same drive/graceful-shutdown shape;
            // only the protocol negotiation differs (issue #378 force-disable escape hatch).
            macro_rules! drive_conn {
                ($conn:expr) => {{
                    let conn = $conn;
                    tokio::pin!(conn);
                    tokio::select! {
                        res = conn.as_mut() => {
                            if let Err(err) = res {
                                error!("Front door connection error: {}", err);
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
                // serve_connection otherwise) — always paired with it below.
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(http_tuning.header_read_timeout)
                    .max_buf_size(http_tuning.max_buf_size);
                drive_conn!(builder.serve_connection(io, service));
            } else {
                let mut builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
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

/// The service body: resolve the route table, then the gateway's own `/__rift/:port` addressing,
/// then a 404 that names itself so a caller can tell "no route matched" from "a route matched but
/// its imposter is gone" (both 404s, different fixes).
async fn handle_front_door_request(
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    routes: Arc<ArcSwap<CompiledRoutes>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = request_host(&req);

    // `load_full` clones the `Arc` rather than holding the `arc_swap::Guard` across the `.await`
    // below (a matched route dispatches into the imposter, which awaits) — the guard is not meant
    // to outlive the borrow it comes from.
    let matched = routes
        .load_full()
        .resolve(
            host.as_deref(),
            req.method(),
            req.uri().path(),
            req.headers(),
        )
        .cloned();

    if let Some(route) = matched {
        return Ok(dispatch_matched_route(route, req, &manager).await);
    }

    // No route claimed it: the single-port gateway's own `/__rift/:port/<path>` addressing still
    // works on this listener (issue #212), so one port serves both styles.
    if let Some(rest) = req.uri().path().strip_prefix("/__rift/") {
        let rest = rest.to_owned();
        let query = req.uri().query().map(str::to_owned);
        return Ok(
            crate::gateway::dispatch_gateway_path(&rest, query.as_deref(), req, &manager).await,
        );
    }

    let message = format!("no route matches {} {}", req.method(), req.uri().path());
    let mut response =
        error_response_typed(StatusCode::NOT_FOUND, ErrorKind::NoSuchResource, &message);
    // Distinguishes this from a matched route whose imposter is gone (also a 404, from
    // `dispatch_to_port`/`dispatch_gateway_path`, which never sets this header) — same status,
    // different fix.
    response.headers_mut().insert(
        HeaderName::from_static(NO_ROUTE_HEADER),
        HeaderValue::from_static("no-route"),
    );
    Ok(response)
}

/// Apply a matched route's rewrites and dispatch to its imposter port.
async fn dispatch_matched_route(
    route: Route,
    mut req: Request<Incoming>,
    manager: &ImposterManager,
) -> Response<Full<Bytes>> {
    let target = route.target;

    if target.strip_prefix
        && let Some(prefix) = route.matches.path_prefix.as_deref()
    {
        match strip_uri_prefix(req.uri(), prefix) {
            Ok(uri) => *req.uri_mut() = uri,
            Err(message) => {
                return error_response_typed(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorKind::InternalError,
                    &message,
                );
            }
        }
    }

    if let Some(new_host) = target.set_host.as_deref() {
        match HeaderValue::from_str(new_host) {
            Ok(value) => {
                req.headers_mut().insert(hyper::header::HOST, value);
            }
            Err(_) => {
                return error_response_typed(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorKind::InternalError,
                    &format!("route target host '{new_host}' is not a valid Host header value"),
                );
            }
        }
    }

    crate::gateway::dispatch_to_port(manager, target.port, req).await
}

/// Strip `prefix` from `uri`'s path, always leaving a valid absolute path — stripping `/api` from
/// exactly `/api` yields `/`, never `""` (an empty path is not a legal request target). The query
/// string is carried over unchanged.
///
/// `prefix` only ever reaches here because [`Route::matches_request`] already matched it
/// segment-aligned against this same path, so the strip always succeeds; the `Err` arm exists so a
/// future change to that invariant fails loudly (a 500) instead of routing to the wrong path.
fn strip_uri_prefix(uri: &Uri, prefix: &str) -> Result<Uri, String> {
    let trimmed_prefix = prefix.trim_end_matches('/');
    let stripped = match uri.path().strip_prefix(trimmed_prefix) {
        Some("") => "/",
        Some(rest) => rest,
        None => {
            return Err(format!(
                "route prefix '{prefix}' does not prefix its own matched path '{}'",
                uri.path()
            ));
        }
    };
    let path_and_query = match uri.query() {
        Some(q) => format!("{stripped}?{q}"),
        None => stripped.to_string(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse().map_err(|e| {
        format!("stripped path '{path_and_query}' is not a valid request target: {e}")
    })?);
    Uri::from_parts(parts).map_err(|e| format!("failed to rebuild URI after stripping prefix: {e}"))
}

/// The request's host: the `Host` header if present, else (for absolute-form requests) the URI
/// authority — with any trailing `:port` removed, so `Host: payments.test:8080` matches a route
/// authored for `payments.test`.
fn request_host(req: &Request<Incoming>) -> Option<String> {
    let raw = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))?;
    Some(strip_host_port(raw).to_string())
}

/// Drop a trailing `:port`, respecting a bracketed IPv6 literal (`[::1]:8080`) whose own colons
/// are not port separators.
fn strip_host_port(host: &str) -> &str {
    if let Some(end) = host.rfind(']') {
        return &host[..=end];
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_host_port_drops_a_plain_port() {
        assert_eq!(strip_host_port("payments.test:8080"), "payments.test");
        assert_eq!(strip_host_port("payments.test"), "payments.test");
    }

    #[test]
    fn strip_host_port_keeps_ipv6_literal_brackets() {
        assert_eq!(strip_host_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_host_port("[::1]"), "[::1]");
    }

    #[test]
    fn strip_uri_prefix_never_produces_an_empty_path() {
        let uri: Uri = "/api".parse().unwrap();
        let stripped = strip_uri_prefix(&uri, "/api").unwrap();
        assert_eq!(stripped.path(), "/");
    }

    #[test]
    fn strip_uri_prefix_keeps_the_query_string() {
        let uri: Uri = "/api/v1/x?q=1".parse().unwrap();
        let stripped = strip_uri_prefix(&uri, "/api/v1").unwrap();
        assert_eq!(stripped.path(), "/x");
        assert_eq!(stripped.query(), Some("q=1"));
    }
}
