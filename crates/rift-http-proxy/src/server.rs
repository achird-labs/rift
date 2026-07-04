//! Server composition as a library (issue #317): the CLI surface, the Mountebank-mode
//! bootstrap, and the metrics server — everything the `rift` binary used to wire privately
//! in `main.rs` — so embedders (issue #203/#310) can compose the standard admin API,
//! config loading, and metrics around their own `ImposterManager` without forking the
//! binary.

use crate::admin_api::{AdminApiServer, DEFAULT_ADMIN_PORT, RunningAdminApi};
use crate::config_loader::{self, ConfigSource};
use crate::extensions::metrics;
use crate::imposter::{ImposterConfig, ImposterManager, TlsDefaults};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, warn};

/// Bounded grace given to in-flight metrics connections on `shutdown()` (issue #342).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Rift - A Mountebank-compatible HTTP chaos engineering proxy
///
/// Rift starts an admin API on port 2525 (configurable) for creating imposters
/// with advanced fault injection, scripting, and stateful testing capabilities.
#[derive(Parser, Debug)]
#[command(name = "rift")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    // === Mountebank-compatible options ===
    /// Port for the admin API (Mountebank mode)
    #[arg(long, default_value_t = DEFAULT_ADMIN_PORT, env = "MB_PORT")]
    pub port: u16,

    /// Hostname to bind the admin API to
    #[arg(long, default_value = "0.0.0.0", env = "MB_HOST")]
    pub host: String,

    /// Load imposters from a config file on startup (JSON or EJS format)
    #[arg(long, value_name = "FILE", env = "MB_CONFIGFILE")]
    pub configfile: Option<PathBuf>,

    /// Directory for persistent imposter storage
    #[arg(long, value_name = "DIR", env = "MB_DATADIR")]
    pub datadir: Option<PathBuf>,

    /// Allow JavaScript injection in responses (for inject and decorate)
    #[arg(long, visible_alias = "allowInjection", env = "MB_ALLOW_INJECTION")]
    pub allow_injection: bool,

    /// Only accept requests from localhost
    #[arg(long, env = "MB_LOCAL_ONLY")]
    pub local_only: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info", env = "MB_LOGLEVEL")]
    pub loglevel: String,

    /// Don't write to log file (stdout only)
    #[arg(long)]
    pub nologfile: bool,

    /// Log file path (default: mb.log in current directory)
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// PID file path
    #[arg(long, value_name = "FILE")]
    pub pidfile: Option<PathBuf>,

    /// CORS allowed origin
    #[arg(long)]
    pub origin: Option<String>,

    /// IP addresses allowed to connect (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub ip_whitelist: Option<Vec<String>>,

    /// Run in mock mode (all imposters are mocks)
    #[arg(long)]
    pub mock: bool,

    /// Enable debug mode
    #[arg(long)]
    pub debug: bool,

    /// Metrics server port
    #[arg(long, default_value = "9090", env = "RIFT_METRICS_PORT")]
    pub metrics_port: u16,

    // === Mountebank compatibility flags (accepted, no-op) ===
    /// Disable EJS template rendering of --configfile (Rift doesn't use EJS; accepted for compatibility)
    #[arg(long, visible_alias = "noParse")]
    pub no_parse: bool,

    /// Custom config formatter module name (Rift auto-detects JSON/YAML; accepted for compatibility)
    #[arg(long)]
    pub formatter: Option<String>,

    /// Custom protocol definitions file (custom protocols not yet supported; accepted for compatibility)
    #[arg(long, value_name = "FILE")]
    pub protofile: Option<PathBuf>,

    /// Require this token in the Authorization header for all admin API requests
    #[arg(long, value_name = "TOKEN", env = "MB_APIKEY")]
    pub api_key: Option<String>,

    /// RC file with default flag values (Mountebank compatibility; partial support — port/host/loglevel only)
    #[arg(long, value_name = "FILE")]
    pub rcfile: Option<PathBuf>,

    /// Default TLS certificate (PEM) for HTTPS imposters that don't carry their own (issue #206)
    #[arg(long, value_name = "FILE", env = "RIFT_DEFAULT_TLS_CERT")]
    pub default_tls_cert: Option<PathBuf>,

    /// Default TLS private key (PEM), paired with --default-tls-cert
    #[arg(long, value_name = "FILE", env = "RIFT_DEFAULT_TLS_KEY")]
    pub default_tls_key: Option<PathBuf>,

    /// Disable the self-signed fallback: an HTTPS imposter without cert material becomes an error
    /// instead of serving with a generated self-signed cert (issue #206)
    #[arg(long, env = "RIFT_NO_SELF_SIGNED_TLS")]
    pub no_self_signed_tls: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the Rift server (default command)
    Start,

    /// Stop a running Rift server
    Stop {
        /// PID file to read for the process to stop
        #[arg(long, default_value = "rift.pid")]
        pidfile: PathBuf,
    },

    /// Restart the Rift server
    Restart {
        /// PID file to read for the process to restart
        #[arg(long, default_value = "rift.pid")]
        pidfile: PathBuf,
    },

    /// Save current imposters to a file
    Save {
        /// Output file path (default: mb.json, matching Mountebank)
        #[arg(long, default_value = "mb.json")]
        savefile: PathBuf,

        /// Strip proxy-recorded stubs (those with recordedFrom set) from the output
        #[arg(long)]
        remove_proxies: bool,
    },

    /// Replay saved imposters
    Replay {
        /// Input file path
        #[arg(long, required = true)]
        configfile: PathBuf,
    },
}

/// Composes the standard Rift server: config loading, metrics, and the admin API,
/// exactly as the `rift` binary wires them (issue #317).
pub struct ServerBuilder {
    cli: Cli,
    manager: Option<Arc<ImposterManager>>,
}

impl ServerBuilder {
    /// Everything the binary derives from the CLI today.
    #[must_use]
    pub fn from_cli(cli: Cli) -> Self {
        Self { cli, manager: None }
    }

    /// Inject a pre-built manager (skipping internal construction, including `--datadir`
    /// write-through and TLS defaults) — the embedding seam.
    #[must_use]
    pub fn manager(mut self, manager: Arc<ImposterManager>) -> Self {
        self.manager = Some(manager);
        self
    }

    /// Load configs, spawn the metrics server, and run the admin API server — the
    /// binary's Mountebank-mode behavior. Runs until the admin server stops or fails.
    pub async fn run(self) -> anyhow::Result<()> {
        self.start().await?.join().await
    }

    /// Everything [`run`](Self::run) does, but returns a [`RunningServer`] once both listeners
    /// are bound instead of serving forever (issue #342) — the embedding seam for a host that
    /// needs the bound addresses (`:0` support) and a graceful shutdown.
    pub async fn start(self) -> anyhow::Result<RunningServer> {
        let cli = self.cli;
        let manager = match self.manager {
            Some(manager) => manager,
            None => {
                // Per-imposter HTTPS defaults (issue #206). The cert/key files are read
                // here rather than in `from_cli` so a missing file fails at run — the
                // same moment the binary fails today.
                let default_cert = cli
                    .default_tls_cert
                    .as_ref()
                    .map(std::fs::read_to_string)
                    .transpose()?;
                let default_key = cli
                    .default_tls_key
                    .as_ref()
                    .map(std::fs::read_to_string)
                    .transpose()?;
                let tls_defaults = TlsDefaults {
                    default_cert,
                    default_key,
                    allow_self_signed: !cli.no_self_signed_tls,
                };
                Arc::new(
                    ImposterManager::with_datadir(cli.datadir.clone())
                        .with_tls_defaults(tls_defaults),
                )
            }
        };

        if let Some(ref configfile) = cli.configfile {
            load_imposters_from_file(&manager, configfile, cli.no_parse).await?;
        }
        if let Some(ref datadir) = cli.datadir {
            load_imposters_from_datadir(&manager, datadir).await?;
        }

        // Bind the metrics server now so a `:0` request can report its port. A bind failure
        // stays non-fatal and only logs — matching the binary, which spawned the metrics
        // server and kept the admin plane up regardless.
        let metrics_addr = SocketAddr::from(([0, 0, 0, 0], cli.metrics_port));
        let metrics = match bind_metrics_server(metrics_addr).await {
            Ok(running) => Some(running),
            Err(e) => {
                error!("Metrics server error: {}", e);
                None
            }
        };

        let host = if cli.local_only {
            "127.0.0.1"
        } else {
            &cli.host
        };
        let addr: SocketAddr = format!("{}:{}", host, cli.port).parse()?;

        info!(
            "Rift Admin API (Mountebank-compatible) starting on http://{}",
            addr
        );
        info!(
            "Metrics available at http://{}:{}/metrics",
            host, cli.metrics_port
        );

        if cli.allow_injection {
            info!("JavaScript injection enabled");
        }

        if cli.formatter.is_some() {
            warn!("--formatter is not supported; Rift auto-detects JSON/YAML config formats");
        }
        if cli.protofile.is_some() {
            warn!("--protofile is not supported; custom protocols are not yet implemented");
        }

        // Retain the config source so POST /admin/reload can re-read it (issue #197).
        // Injection gating is threaded explicitly (issue #342) rather than read from env.
        let mut server = AdminApiServer::new(addr, manager, cli.api_key)
            .with_allow_injection(cli.allow_injection);
        if let Some(configfile) = cli.configfile {
            server = server.with_config_source(ConfigSource::File {
                path: configfile,
                no_parse: cli.no_parse,
            });
        } else if let Some(datadir) = cli.datadir {
            server = server.with_config_source(ConfigSource::Dir(datadir));
        }
        let admin = server.bind().await?;
        Ok(RunningServer { admin, metrics })
    }
}

/// A bound, running Rift server (issue #342): the admin API plus an optional metrics server.
/// Reports both bound addresses and shuts both down gracefully.
pub struct RunningServer {
    admin: RunningAdminApi,
    metrics: Option<RunningMetrics>,
}

impl RunningServer {
    /// The bound admin API address (resolves a `:0` request to the assigned port).
    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.local_addr()
    }

    /// The bound metrics address, if the metrics server bound successfully.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics.as_ref().map(RunningMetrics::local_addr)
    }

    /// Run until the admin API accept loop exits — the binary's `run()` behavior. The metrics
    /// server keeps serving in the background, as it did under the previous `tokio::spawn`.
    pub async fn join(self) -> anyhow::Result<()> {
        self.admin.join().await
    }

    /// Stop accepting on both listeners, giving in-flight connections a bounded grace.
    pub async fn shutdown(self) {
        self.admin.shutdown().await;
        if let Some(metrics) = self.metrics {
            metrics.shutdown().await;
        }
    }
}

/// Serve Prometheus metrics at `GET /metrics` on `addr` (anything else is a 404).
/// Runs until the listener fails; callers normally `tokio::spawn` it. Delegates to
/// [`bind_metrics_server`] + [`RunningMetrics::join`] so the binary path is unchanged.
pub async fn run_metrics_server(addr: SocketAddr) -> anyhow::Result<()> {
    bind_metrics_server(addr).await?.join().await
}

/// Bind the metrics listener (`:0` is fine) and start serving, returning a handle that reports
/// the bound address and can be shut down gracefully (issue #342).
pub async fn bind_metrics_server(addr: SocketAddr) -> anyhow::Result<RunningMetrics> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!("Metrics server listening on http://{}/metrics", local_addr);

    let cancel = CancellationToken::new();
    let tracker = TaskTracker::new();
    let (loop_cancel, loop_tracker) = (cancel.clone(), tracker.clone());
    let task = tokio::spawn(async move {
        let result = metrics_accept_loop(listener, loop_cancel, loop_tracker).await;
        // Preserve the pre-#342 behavior: an accept-loop failure is logged. Otherwise it would
        // only surface via join(), which RunningServer does not call for the metrics task.
        if let Err(ref e) = result {
            error!("Metrics server error: {}", e);
        }
        result
    });

    Ok(RunningMetrics {
        local_addr,
        cancel,
        tracker,
        task: Mutex::new(Some(task)),
    })
}

/// A bound, running metrics server (issue #342).
pub struct RunningMetrics {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    tracker: TaskTracker,
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

impl RunningMetrics {
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
            .expect("metrics task mutex poisoned")
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
                "Metrics server shutdown: in-flight connections did not drain within the grace period"
            );
        }
    }

    /// Run until the accept loop exits (returns immediately if already shut down).
    pub async fn join(self) -> anyhow::Result<()> {
        let task = self
            .task
            .lock()
            .expect("metrics task mutex poisoned")
            .take();
        match task {
            Some(task) => match task.await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("metrics server task failed: {join_err}")),
            },
            None => Ok(()),
        }
    }
}

async fn metrics_accept_loop(
    listener: TcpListener,
    cancel: CancellationToken,
    tracker: TaskTracker,
) -> anyhow::Result<()> {
    use hyper::service::service_fn;
    use hyper::{Request, Response, body::Incoming};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;

    loop {
        let (stream, _) = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted?,
        };
        let io = TokioIo::new(stream);
        let conn_cancel = cancel.clone();

        tracker.spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| async move {
                if req.uri().path() == "/metrics" {
                    let metrics = metrics::collect_metrics();
                    Ok::<_, Infallible>(Response::new(metrics))
                } else {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(404)
                            .body("Not Found\n".to_string())
                            .expect("static response is infallible"),
                    )
                }
            });

            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let conn = builder.serve_connection(io, service);
            tokio::pin!(conn);
            tokio::select! {
                res = conn.as_mut() => {
                    if let Err(err) = res {
                        error!("Metrics server connection error: {}", err);
                    }
                }
                _ = conn_cancel.cancelled() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
    }
    Ok(())
}

/// Load imposters from a JSON config file
async fn load_imposters_from_file(
    manager: &Arc<ImposterManager>,
    path: &PathBuf,
    no_parse: bool,
) -> anyhow::Result<()> {
    info!("Loading imposters from configfile: {:?}", path);

    let configs = config_loader::load_configs(&ConfigSource::File {
        path: path.clone(),
        no_parse,
    })?;

    for config in configs {
        info!(
            "Creating imposter on port {:?} from configfile",
            config.port
        );
        match manager.create_imposter(config).await {
            Ok(port) => info!("Created imposter on port {}", port),
            Err(e) => error!("Failed to create imposter: {}", e),
        }
    }

    Ok(())
}

/// Load imposters from a data directory
async fn load_imposters_from_datadir(
    manager: &Arc<ImposterManager>,
    datadir: &PathBuf,
) -> anyhow::Result<()> {
    info!("Loading imposters from datadir: {:?}", datadir);

    if !datadir.exists() {
        std::fs::create_dir_all(datadir)?;
        return Ok(());
    }

    for entry in std::fs::read_dir(datadir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let content = std::fs::read_to_string(&path)?;
            if let Ok(config) = serde_json::from_str::<ImposterConfig>(&content) {
                info!("Loading imposter on port {:?} from {:?}", config.port, path);
                match manager.create_imposter(config).await {
                    Ok(port) => info!("Created imposter on port {} from {:?}", port, path),
                    Err(e) => error!("Failed to create imposter from {:?}: {}", path, e),
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_parse_flag_accepted() {
        let cli = Cli::try_parse_from(["rift", "--noParse"]).expect("--noParse should be accepted");
        assert!(cli.no_parse);
    }

    #[test]
    fn test_no_parse_snake_case_accepted() {
        let cli =
            Cli::try_parse_from(["rift", "--no-parse"]).expect("--no-parse should be accepted");
        assert!(cli.no_parse);
    }

    #[test]
    fn test_formatter_flag_accepted() {
        let cli = Cli::try_parse_from(["rift", "--formatter", "mountebank-formatters"])
            .expect("--formatter should be accepted");
        assert_eq!(cli.formatter.as_deref(), Some("mountebank-formatters"));
    }

    #[test]
    fn test_protofile_flag_accepted() {
        let cli = Cli::try_parse_from(["rift", "--protofile", "protocols.json"])
            .expect("--protofile should be accepted");
        assert_eq!(
            cli.protofile.as_deref(),
            Some(std::path::Path::new("protocols.json"))
        );
    }

    #[test]
    fn test_log_flag_parsed() {
        let cli = Cli::try_parse_from(["rift", "--log", "/tmp/test.log"])
            .expect("--log should be accepted");
        assert_eq!(cli.log, Some(std::path::PathBuf::from("/tmp/test.log")));
    }

    #[test]
    fn test_nologfile_flag_parsed() {
        let cli =
            Cli::try_parse_from(["rift", "--nologfile"]).expect("--nologfile should be accepted");
        assert!(cli.nologfile);
    }

    #[test]
    fn test_nologfile_default_is_false() {
        let cli = Cli::try_parse_from(["rift"]).expect("default parse");
        assert!(!cli.nologfile);
        assert!(cli.log.is_none());
    }
}
