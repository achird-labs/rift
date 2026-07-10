//! Admin API server.

use crate::admin_api::router::route_request;
use crate::config_loader::ConfigSource;
use crate::extensions::decorate::{ResponsePhase, with_annotation_scope};
use crate::imposter::ImposterManager;
use crate::intercept_control::InterceptControl;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info};

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
        let task = tokio::spawn(async move {
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
                tracing::error!("Admin API server error: {}", e);
            }
            result
        });

        Ok(RunningAdminApi {
            local_addr,
            cancel,
            tracker,
            task: Mutex::new(Some(task)),
        })
    }

    /// Run the admin API server until the accept loop exits. Delegates to `bind` + `join`
    /// so the binary path is byte-identical to binding then serving forever.
    pub async fn run(self) -> Result<(), anyhow::Error> {
        self.bind().await?.join().await
    }
}

/// A bound, running admin API server (issue #342). Reports its listening address and offers a
/// graceful shutdown that does not require dropping the runtime.
pub struct RunningAdminApi {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
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
    }

    /// Run until the accept loop exits (returns immediately if already shut down).
    pub async fn join(self) -> anyhow::Result<()> {
        match take_task(&self.task) {
            Some(task) => match task.await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("admin API task failed: {join_err}")),
            },
            None => Ok(()),
        }
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
    config_source: Option<Arc<ConfigSource>>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
    cancel: CancellationToken,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted?,
        };
        let io = TokioIo::new(stream);
        let manager = Arc::clone(&manager);
        let api_key = api_key.clone();
        let config_source = config_source.clone();
        let intercept = intercept.clone();
        let scripts_dir = scripts_dir.clone();
        let conn_cancel = cancel.clone();

        tracker.spawn(async move {
            let service = service_fn(move |req| {
                let manager = Arc::clone(&manager);
                let api_key = api_key.clone();
                let config_source = config_source.clone();
                let intercept = intercept.clone();
                let scripts_dir = scripts_dir.clone();
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
                            if auth != key.as_str() {
                                return Ok::<_, hyper::Error>(unauthorized_response());
                            }
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

            if rift_core::util::http2_disabled() {
                drive_conn!(
                    hyper::server::conn::http1::Builder::new().serve_connection(io, service)
                );
            } else {
                let builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                drive_conn!(builder.serve_connection(io, service));
            }
        });
    }
    Ok(())
}

fn unauthorized_response() -> Response<Full<Bytes>> {
    let body = r#"{"errors":[{"code":"unauthorized","message":"Invalid authorization token"}]}"#;
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
}
