//! Server composition as a library (issue #317): the CLI surface, the Mountebank-mode
//! bootstrap, and the metrics server — everything the `rift` binary used to wire privately
//! in `main.rs` — so embedders (issue #203/#310) can compose the standard admin API,
//! config loading, and metrics around their own `ImposterManager` without forking the
//! binary.

use crate::admin_api::{AdminApiServer, DEFAULT_ADMIN_PORT, RunningAdminApi};
use crate::config_loader::{self, ConfigSource};
use crate::extensions::metrics;
use crate::imposter::{
    ImposterConfig, ImposterManager, ScriptBaseDir, TlsDefaults, resolve_scripts,
};
use crate::injection_gate::GATED_SCRIPT_SURFACES;
use crate::intercept_control::{InterceptControl, InterceptStartOptions};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, warn};

/// Bounded grace given to in-flight metrics connections on `shutdown()` (issue #342).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Default `rift healthcheck` timeout. Deliberately *under* the `--timeout=3s` the images'
/// HEALTHCHECK lines allow, so a hung server makes the probe report an unhealthy verdict itself
/// rather than race Docker's killer for it (equal budgets fire at the same instant).
pub const DEFAULT_HEALTHCHECK_TIMEOUT_SECS: u64 = 2;

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

    /// Root directory `_rift.script` `file:` references resolve under for admin-API-created
    /// imposters (issue #356). A resolved path that escapes this root is rejected. Without it,
    /// admin-API `file:` script references are rejected outright (`--configfile`/`--datadir`
    /// loads are unaffected — those resolve relative to the config's own directory).
    #[arg(long, value_name = "DIR", env = "RIFT_SCRIPTS_DIR")]
    pub scripts_dir: Option<PathBuf>,

    /// Allow JavaScript injection in responses (for inject and decorate)
    #[arg(long, visible_alias = "allowInjection", env = "MB_ALLOW_INJECTION")]
    pub allow_injection: bool,

    /// Only accept requests from localhost
    #[arg(long, env = "MB_LOCAL_ONLY")]
    pub local_only: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info", env = "MB_LOGLEVEL")]
    pub loglevel: String,

    /// Runtime topology: work-stealing (default) or per-core[=N] — N single-threaded worker
    /// runtimes with SO_REUSEPORT sharded accept (RFC-712; experimental, Linux-first: macOS
    /// falls back to work-stealing with a warning, Windows rejects it)
    #[arg(long, value_name = "MODE", env = "RIFT_RUNTIME")]
    pub runtime: Option<String>,

    /// Pin per-core worker threads to CPU cores (only meaningful with --runtime per-core;
    /// effective on Linux, advisory elsewhere)
    #[arg(long, env = "RIFT_RUNTIME_AFFINITY")]
    pub runtime_affinity: bool,

    /// Don't write to log file (stdout only)
    #[arg(long)]
    pub nologfile: bool,

    /// Log file path (default: mb.log in current directory)
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// PID file path. `global` so `stop`/`restart` bind the same value whether it is given
    /// before or after the subcommand (issue #827). Deliberately has NO `default_value`: a default
    /// here would make every plain `rift` start write `./rift.pid`. `stop`/`restart` apply
    /// [`bootstrap::DEFAULT_PIDFILE`](crate::bootstrap::DEFAULT_PIDFILE) at their dispatch site.
    #[arg(long, value_name = "FILE", global = true)]
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

    /// Start a TLS-MITM intercept/redirect proxy listener on this port (epic #394). Off when
    /// unset. Configure rules and export the CA via the admin API's `/intercept/*` routes.
    #[arg(long, value_name = "PORT", env = "RIFT_INTERCEPT_PORT")]
    pub intercept_port: Option<u16>,

    /// PEM CA certificate for interception. Used with `--intercept-ca-key`; a CA is generated
    /// in-memory when both are omitted.
    #[arg(long, value_name = "FILE", env = "RIFT_INTERCEPT_CA_CERT")]
    pub intercept_ca_cert: Option<PathBuf>,

    /// PEM CA private key for interception. Required together with `--intercept-ca-cert`.
    #[arg(long, value_name = "FILE", env = "RIFT_INTERCEPT_CA_KEY")]
    pub intercept_ca_key: Option<PathBuf>,

    /// Inline CA certificate PEM for interception (issue #593) — the PEM text itself, not a path.
    /// Used with `--intercept-ca-key-pem`; conflicts with the `--intercept-ca-cert`/`-key` file
    /// pair. Env is the intended vehicle (`RIFT_INTERCEPT_CA_CERT_PEM`).
    #[arg(
        long,
        value_name = "PEM",
        env = "RIFT_INTERCEPT_CA_CERT_PEM",
        conflicts_with = "intercept_ca_cert",
        conflicts_with = "intercept_ca_key"
    )]
    pub intercept_ca_cert_pem: Option<String>,

    /// Inline CA private-key PEM for interception (issue #593). Required together with
    /// `--intercept-ca-cert-pem`; conflicts with the CA file pair.
    #[arg(
        long,
        value_name = "PEM",
        env = "RIFT_INTERCEPT_CA_KEY_PEM",
        conflicts_with = "intercept_ca_cert",
        conflicts_with = "intercept_ca_key"
    )]
    pub intercept_ca_key_pem: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Start the Rift server (default command)
    Start,

    /// Stop a running Rift server
    Stop,

    /// Restart the Rift server
    Restart,

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

    /// Validate or run a script outside a running server (issue #360)
    Script {
        #[command(subcommand)]
        action: ScriptAction,
    },

    /// Probe a running server's admin API; exits 0 when healthy, 1 otherwise (issue #664).
    ///
    /// This is the container HEALTHCHECK: the `-static` image is `FROM scratch`, so there is no
    /// shell and no curl to probe with.
    Healthcheck {
        /// URL to probe (default: the admin API's /health on --host/--port)
        #[arg(long, value_name = "URL")]
        url: Option<String>,

        /// Give up after this many seconds
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_HEALTHCHECK_TIMEOUT_SECS)]
        timeout: u64,
    },
}

/// `rift script <check|run>` (issue #360): scripting DX tools that need neither an admin API nor
/// a running imposter — everything runs synchronously, in-process, against a fixture.
#[derive(Subcommand, Debug, Clone)]
pub enum ScriptAction {
    /// Statically validate a script or config file: engine syntax, entrypoint presence/arity for
    /// the intended hook, v1-shape deprecation, and (for a config) state-used-without-flowState.
    /// No server is started. Exits non-zero on any error.
    Check {
        /// A raw script file (`.rhai`/`.js`) or a rift config file (JSON/YAML)
        /// containing `_rift.script` entries.
        target: PathBuf,

        /// Which entrypoint hook to check a raw script file against
        /// (`respond`/`matches`/`transform`/`delay`). Ignored for a config file target — every
        /// `_rift.script` there is a response-position script, i.e. always `respond`.
        #[arg(long, default_value = "respond")]
        hook: String,
    },

    /// Execute a script against a fixture request and seeded flow state — no server running.
    /// Prints the decision, the mutated flow state, captured `ctx.logger` output, and the
    /// execution duration.
    Run {
        /// Script file (`.rhai`/`.js`).
        target: PathBuf,

        /// JSON file with the request-object shape scripts see:
        /// `{method, path, headers, query, pathParams, body}`. All fields are optional; an
        /// empty `GET /` with no headers/body is used when this flag is omitted entirely.
        #[arg(long)]
        request: Option<PathBuf>,

        /// Seed flow state before running: `key=value`, repeatable. The value is parsed as JSON
        /// when it parses (numbers/bools/objects/arrays/quoted strings); otherwise it's stored
        /// as a plain string.
        #[arg(long = "state", value_name = "KEY=VALUE")]
        state: Vec<String>,

        /// Flow id the seeded state and the script's `ctx.state`/`flow_store` calls use.
        #[arg(long, default_value = "cli")]
        flow_id: String,

        /// Script engine (`rhai`/`js`). Inferred from the file extension when omitted.
        #[arg(long)]
        engine: Option<String>,

        /// Entrypoint hook to run. Only `respond` is wired end-to-end for both engines
        /// today (`matches`/`transform`/`delay` are Rhai-only and not yet reachable outside the
        /// engine's own unit tests) — any other value is a clean error, not a panic.
        #[arg(long, default_value = "respond")]
        hook: String,
    },
}

/// Composes the standard Rift server: config loading, metrics, and the admin API,
/// exactly as the `rift` binary wires them (issue #317).
pub struct ServerBuilder {
    cli: Cli,
    manager: Option<Arc<ImposterManager>>,
    accept_runtimes: Vec<tokio::runtime::Handle>,
}

impl ServerBuilder {
    /// Everything the binary derives from the CLI today.
    #[must_use]
    pub fn from_cli(cli: Cli) -> Self {
        Self {
            cli,
            manager: None,
            accept_runtimes: Vec::new(),
        }
    }

    /// Fan imposter accept loops out across per-core worker runtimes (RFC-712, issue #745).
    /// Applies to the internally-constructed manager only; a manager injected via
    /// [`Self::manager`] carries its own topology (`ImposterManager::with_accept_runtimes`).
    /// An empty vec keeps the default single-listener topology.
    #[must_use]
    pub fn accept_runtimes(mut self, runtimes: Vec<tokio::runtime::Handle>) -> Self {
        self.accept_runtimes = runtimes;
        self
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
                        .with_tls_defaults(tls_defaults)
                        .with_accept_runtimes(self.accept_runtimes),
                )
            }
        };

        let mut intercept_block = None;
        if let Some(ref configfile) = cli.configfile {
            intercept_block = load_imposters_from_file(
                &manager,
                configfile,
                cli.no_parse,
                cli.allow_injection,
                &cli_intercept_flags(&cli),
            )
            .await?;
        }
        if let Some(ref datadir) = cli.datadir {
            load_imposters_from_datadir(&manager, datadir, cli.allow_injection).await?;
        }

        // Bind the metrics server now so a `:0` request can report its port. A bind failure
        // stays non-fatal and only logs — matching the binary, which spawned the metrics
        // server and kept the admin plane up regardless.
        let metrics_addr = SocketAddr::from(([0, 0, 0, 0], cli.metrics_port));
        let metrics = match bind_metrics_server(metrics_addr).await {
            Ok(running) => Some(running),
            Err(e) => {
                error!("Metrics server error: {e:#}");
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
        if let Some(scripts_dir) = cli.scripts_dir {
            server = server.with_scripts_dir(scripts_dir);
        }
        if let Some(configfile) = cli.configfile {
            server = server.with_config_source(ConfigSource::File {
                path: configfile,
                no_parse: cli.no_parse,
            });
        } else if let Some(datadir) = cli.datadir {
            server = server.with_config_source(ConfigSource::Dir(datadir));
        }

        // Intercept/TLS-MITM listener (epic #394 + runtime lifecycle #493). The control slot is
        // always created and handed to the admin server, so `POST/GET/DELETE /intercept` work on
        // every standalone server — flag or no flag (the issue's goal for the connect transport).
        // `--intercept-port` just eagerly starts the same listener the API would; a start error
        // still aborts startup, as before.
        // The config file's `intercept` block (issue #655) and `--intercept-port` are two spellings
        // of the same listener; supplying both was already refused when the file was loaded, so at
        // most one of these arms can run. The block declares its own bind host, so unlike the flag
        // it does not inherit the admin `host`.
        let intercept = InterceptControl::default();
        let start_options = intercept_block.or_else(|| {
            cli.intercept_port
                .map(|intercept_port| InterceptStartOptions {
                    host: Some(host.to_string()),
                    port: Some(intercept_port),
                    ca_cert_path: cli
                        .intercept_ca_cert
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    ca_key_path: cli
                        .intercept_ca_key
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    ca_cert_pem: cli.intercept_ca_cert_pem.clone(),
                    ca_key_pem: cli.intercept_ca_key_pem.clone(),
                    // Cloned (not moved) because `cli` is borrowed for the rest of startup.
                    ..Default::default()
                })
        });
        if let Some(options) = start_options {
            let seeded_rules = options.rules.len();
            if let Err(e) = intercept.start(options).await {
                // Same contract as the admin-bind arm below: don't orphan a listener already bound
                // by this call. #655 widens the ways this can fail (rule-capacity, and a CA/bind
                // error declared by the config block rather than typed as a flag), and `start()` is
                // an embedding seam callers retry — a held metrics port would fail the retry too.
                if let Some(metrics) = metrics {
                    metrics.shutdown().await;
                }
                return Err(anyhow::anyhow!("intercept: {e}"));
            }
            info!(
                "Rift intercept proxy listening (HTTPS forward-proxy) on {} with {} configured rule(s)",
                intercept
                    .status()
                    .expect("intercept listener bound after a successful start"),
                seeded_rules
            );
        }
        server = server.with_intercept(intercept.clone());

        let admin = match server.bind().await {
            Ok(admin) => admin,
            Err(e) => {
                // Don't orphan the listeners already started if the admin bind fails — start() is
                // an embedding seam and callers may retry after an error.
                intercept.stop().await;
                if let Some(metrics) = metrics {
                    metrics.shutdown().await;
                }
                return Err(e);
            }
        };
        Ok(RunningServer {
            admin,
            metrics,
            intercept,
        })
    }
}

/// A bound, running Rift server (issue #342): the admin API plus an optional metrics server.
/// Reports both bound addresses and shuts both down gracefully.
pub struct RunningServer {
    admin: RunningAdminApi,
    metrics: Option<RunningMetrics>,
    intercept: InterceptControl,
}

impl RunningServer {
    /// Build a `RunningServer` whose admin accept loop is an arbitrary future — the seam for
    /// testing what your code does when the admin plane dies (issue #825).
    ///
    /// An embedder that races [`wait`](Self::wait) and propagates the outcome to process exit
    /// (rift-enterprise #42) cannot otherwise reach that `Err` path from outside this crate: the
    /// real accept loop only fails by genuinely breaking the listener. Pass a future returning
    /// `Err` (or one that panics) and assert your reaction.
    ///
    /// No listener is bound and there is no metrics server; `admin_addr()` reports `127.0.0.1:0`.
    /// Gated behind the `test-util` feature — test scaffolding, not a production constructor.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_admin_accept_task<F>(loop_body: F) -> Self
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self {
            admin: crate::admin_api::RunningAdminApi::with_accept_task(loop_body),
            metrics: None,
            intercept: InterceptControl::default(),
        }
    }
    /// The bound admin API address (resolves a `:0` request to the assigned port).
    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.local_addr()
    }

    /// The bound metrics address, if the metrics server bound successfully.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics.as_ref().map(RunningMetrics::local_addr)
    }

    /// The bound intercept-proxy address, if an intercept listener is running — whether started by
    /// `--intercept-port` or later over `POST /intercept` (resolves a `:0` request to the assigned
    /// port).
    pub fn intercept_addr(&self) -> Option<SocketAddr> {
        self.intercept.status()
    }

    /// Run until the admin API accept loop exits — the binary's `run()` behavior. The metrics
    /// server keeps serving in the background, as it did under the previous `tokio::spawn`.
    pub async fn join(self) -> anyhow::Result<()> {
        self.admin.join().await
    }

    /// Run until the admin API accept loop exits, **without consuming the server** (issue #806).
    ///
    /// This is the seam for an embedder that must race the server against its own shutdown signal:
    /// `join` moves the server, so the signal arm could no longer reach `shutdown`.
    ///
    /// ```no_run
    /// # async fn example(server: rift_http_proxy::server::RunningServer) -> anyhow::Result<()> {
    /// # async fn termination_signal() {}
    /// tokio::select! {
    ///     result = server.wait() => return result,   // the admin plane died — surface it
    ///     () = termination_signal() => {}
    /// }
    /// server.shutdown().await;                      // still owned, still shutdownable
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The accept loop's error is delivered to the first caller only; later calls return `Ok(())`.
    /// Like `join`, this tracks the admin plane — the metrics server keeps serving in background.
    pub async fn wait(&self) -> anyhow::Result<()> {
        self.admin.wait().await
    }

    /// Stop accepting on all listeners, giving in-flight connections a bounded grace. Stops
    /// whatever intercept listener is running at shutdown time, including one started over the API
    /// after this server was bound.
    ///
    /// Takes `&self` (issue #806) so it composes with [`wait`](Self::wait) and can be called
    /// through a shared handle; every underlying shutdown is already idempotent.
    pub async fn shutdown(&self) {
        self.admin.shutdown().await;
        if let Some(metrics) = &self.metrics {
            metrics.shutdown().await;
        }
        self.intercept.stop().await;
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
            error!("Metrics server error: {e:#}");
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
    use rift_mock_core::proxy::{
        AcceptBackoff, AcceptErrorClass, AcceptErrorLog, HttpTuning, classify_accept_error,
        is_fatal_listener_error,
    };
    use std::convert::Infallible;

    // Read HTTP tuning once per listener, not per accepted connection.
    let http_tuning = HttpTuning::from_env();
    // `None` (the default) preserves today's behavior exactly: no semaphore, no permit, accept as
    // fast as the kernel hands connections over (issue #716).
    let connection_semaphore = http_tuning
        .max_connections
        .map(|n| std::sync::Arc::new(tokio::sync::Semaphore::new(n)));

    // Accept-error handling, identical to the admin loop's (issues #826, #834). Previously any
    // `accept()` failure propagated out and ended the metrics server — and since `RunningServer`
    // never joins this task, the death was a single log line and `/metrics` simply stopped
    // answering until the process restarted.
    let mut backoff = AcceptBackoff::new();
    let mut error_log = AcceptErrorLog::default();

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
                    info!(
                        suppressed,
                        "metrics accept loop recovered after {suppressed} suppressed error(s)"
                    );
                    backoff.reset();
                }
                accepted
            }
            // A broken listener fd cannot be cured by waiting; end the loop so the failure reaches
            // the `Metrics server error` log rather than spinning forever (issue #834).
            Err(e) if is_fatal_listener_error(&e) => {
                return Err(anyhow::anyhow!(
                    "metrics listener is unusable, giving up: {e}"
                ));
            }
            Err(e) => match classify_accept_error(&e) {
                AcceptErrorClass::Transient => {
                    debug!("transient accept error on the metrics listener: {e}");
                    continue;
                }
                AcceptErrorClass::Systemic => {
                    if error_log.on_error() {
                        error!(
                            "accept error on the metrics listener: {e}; backing off \
                             (further errors suppressed until recovery)"
                        );
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

        tracker.spawn(async move {
            // Held for the connection's lifetime; released back to the semaphore when this task
            // ends (issue #716).
            let _permit = permit;
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

            // Both builders yield a Connection with the same drive/graceful-shutdown shape;
            // only the protocol negotiation differs (issue #378 force-disable escape hatch).
            macro_rules! drive_conn {
                ($conn:expr) => {{
                    let conn = $conn;
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

/// The startup error for a `--configfile` whose imposters need `--allowInjection` (issue #612),
/// or `None` when every imposter is admissible. One message listing every offender, so the
/// operator fixes the file in a single pass instead of restarting into the next error.
fn configfile_injection_error(
    path: &Path,
    configs: &[ImposterConfig],
    allow_injection: bool,
) -> Option<String> {
    if allow_injection {
        return None;
    }
    let offenders = crate::injection_gate::gated_offender_ports(configs);
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        "{}: imposter(s) on port(s) {} use {GATED_SCRIPT_SURFACES}. \
         Restart with --allowInjection to allow this, or remove the scripting from the config.",
        path.display(),
        offenders.join(", "),
    ))
}

/// Split parsed datadir configs into those that may be served and those gated by `--allowInjection`
/// (issue #612). A datadir file is skipped rather than fatal: `{port}.json` is persisted from
/// admin-API writes, so a leftover script-bearing file from an earlier `--allowInjection` run must
/// fail closed without bricking startup for every other imposter.
fn partition_gated_datadir(
    parsed: Vec<LoadedImposter>,
    allow_injection: bool,
) -> (Vec<LoadedImposter>, Vec<SkippedImposterFile>) {
    if allow_injection {
        return (parsed, Vec::new());
    }
    let (gated, servable): (Vec<_>, Vec<_>) = parsed
        .into_iter()
        .partition(|(_, config)| crate::injection_gate::config_uses_script_surface(config));
    let gated = gated
        .into_iter()
        .map(|(path, _)| SkippedImposterFile {
            path,
            reason: format!("uses {GATED_SCRIPT_SURFACES}, which require --allowInjection"),
        })
        .collect();
    (servable, gated)
}

/// The `--intercept-*` flags the operator supplied, by long name; empty when none were.
fn cli_intercept_flags(cli: &Cli) -> Vec<&'static str> {
    [
        ("--intercept-port", cli.intercept_port.is_some()),
        ("--intercept-ca-cert", cli.intercept_ca_cert.is_some()),
        ("--intercept-ca-key", cli.intercept_ca_key.is_some()),
        (
            "--intercept-ca-cert-pem",
            cli.intercept_ca_cert_pem.is_some(),
        ),
        ("--intercept-ca-key-pem", cli.intercept_ca_key_pem.is_some()),
    ]
    .into_iter()
    .filter(|(_, present)| *present)
    .map(|(name, _)| name)
    .collect()
}

/// The error for a config `intercept` block supplied alongside `--intercept-*` flags, or `None`
/// when at most one source configures the listener (issue #655).
///
/// Two spellings of one listener is a config bug, so it is refused rather than resolved by a silent
/// precedence rule the operator would have to know — the same fail-loud choice clap already makes
/// for the CA path/PEM pairs.
fn intercept_source_conflict_error(path: &Path, flags: &[&str]) -> Option<String> {
    if flags.is_empty() {
        return None;
    }
    Some(format!(
        "{}: the `intercept` block and {} both configure the intercept listener. \
         Use one or the other — the block declares the listener, its CA, and its rules together. \
         (Each of these flags also has a RIFT_INTERCEPT_* environment variable, which is the \
         intended vehicle for the inline CA PEMs — check the environment, not just the command line.)",
        path.display(),
        flags.join(", ")
    ))
}

/// The error for config-file intercept rules carrying a gated script surface, or `None` when every
/// rule is admissible (issue #655). One message listing every offender, matching
/// [`configfile_injection_error`]'s contract for imposters.
fn configfile_intercept_injection_error(
    path: &Path,
    rules: &[crate::intercept_rules::InterceptRule],
    allow_injection: bool,
) -> Option<String> {
    if allow_injection {
        return None;
    }
    let offenders: Vec<String> = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| crate::injection_gate::intercept_rule_uses_script_surface(rule))
        .map(|(index, rule)| match &rule.host {
            Some(host) => format!("#{index} ({host})"),
            None => format!("#{index} (any host)"),
        })
        .collect();
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        "{}: intercept rule(s) {} use an inject predicate, which requires --allowInjection. \
         Remove the injection or restart with --allowInjection.",
        path.display(),
        offenders.join(", ")
    ))
}

/// Load imposters from a JSON config file, returning the file's optional `intercept` block for the
/// caller to start the listener from (issue #655) — the block is parsed and validated here so the
/// whole document is refused as one, before any imposter exists.
async fn load_imposters_from_file(
    manager: &Arc<ImposterManager>,
    path: &PathBuf,
    no_parse: bool,
    allow_injection: bool,
    intercept_flags: &[&str],
) -> anyhow::Result<Option<InterceptStartOptions>> {
    info!("Loading imposters from configfile: {:?}", path);

    let loaded = config_loader::load_configs_full(&ConfigSource::File {
        path: path.clone(),
        no_parse,
    })?;

    // Refuse before creating anything: a gated configfile must not half-load (issue #612). The
    // intercept block is validated in the same breath, so a file that cannot bring up its listener
    // never half-applies its imposters either.
    if let Some(message) = configfile_injection_error(path, &loaded.imposters, allow_injection) {
        anyhow::bail!(message);
    }
    if let Some(block) = &loaded.intercept {
        if let Some(message) = intercept_source_conflict_error(path, intercept_flags) {
            anyhow::bail!(message);
        }
        if let Some(message) =
            configfile_intercept_injection_error(path, &block.rules, allow_injection)
        {
            anyhow::bail!(message);
        }
    }

    for config in loaded.imposters {
        info!(
            "Creating imposter on port {:?} from configfile",
            config.port
        );
        match manager.create_imposter(config).await {
            Ok(port) => info!("Created imposter on port {}", port),
            Err(e) => error!("Failed to create imposter: {}", e),
        }
    }

    Ok(loaded.intercept)
}

/// Load imposters from a data directory
/// A datadir `*.json` file that could not be turned into a served imposter, kept so the loader can
/// surface all of them together instead of dropping each with only a per-file log line (issue #532).
struct SkippedImposterFile {
    path: PathBuf,
    reason: String,
}

/// A successfully-parsed datadir imposter paired with the file it came from (so creation-phase logs
/// and skips can name the offending file).
type LoadedImposter = (PathBuf, ImposterConfig);

/// Render an operator-visible summary of skipped datadir files, or `None` when nothing was skipped.
/// Emitted at `error!` level (not the old per-file `warn!`) so a typo'd fixture that silently
/// vanished from the running set is impossible to miss in the startup output (issue #532).
fn format_skipped_summary(skipped: &[SkippedImposterFile]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let mut summary = format!(
        "{} imposter file(s) in the datadir were skipped and are NOT being served:",
        skipped.len()
    );
    for file in skipped {
        summary.push_str(&format!("\n  - {}: {}", file.path.display(), file.reason));
    }
    Some(summary)
}

/// Read and parse every `*.json` in `datadir`, resolving each imposter's scripts. Returns the
/// successfully-parsed `(path, config)` pairs plus a list of files that could not be parsed into an
/// imposter config (an unreadable directory entry, an unreadable file, invalid JSON, or unresolvable
/// scripts). A single bad file is collected rather than dropped or propagated, so it neither
/// vanishes silently nor aborts loading of the remaining valid imposters (issue #532). Only a
/// failure to open the directory itself is fatal.
fn read_and_parse_datadir(
    datadir: &Path,
    base: &ScriptBaseDir,
) -> anyhow::Result<(Vec<LoadedImposter>, Vec<SkippedImposterFile>)> {
    let mut parsed = Vec::new();
    let mut skipped = Vec::new();

    for entry in std::fs::read_dir(datadir)? {
        // A single unreadable directory entry (e.g. a mid-iteration removal or a stat quirk) must
        // not abort loading of every other imposter — collect it like any other skipped file.
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                skipped.push(SkippedImposterFile {
                    path: datadir.to_path_buf(),
                    reason: format!("could not read a directory entry: {e}"),
                });
                continue;
            }
        };
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(e) => {
                    skipped.push(SkippedImposterFile {
                        path,
                        reason: format!("could not read file: {e}"),
                    });
                    continue;
                }
            };
            match serde_json::from_str::<ImposterConfig>(&content) {
                Ok(mut config) => {
                    if let Err(e) = resolve_scripts(&mut config, base) {
                        skipped.push(SkippedImposterFile {
                            path,
                            reason: format!("script resolution failed: {e}"),
                        });
                        continue;
                    }
                    parsed.push((path, config));
                }
                Err(e) => skipped.push(SkippedImposterFile {
                    path,
                    reason: format!("invalid imposter JSON: {e}"),
                }),
            }
        }
    }

    Ok((parsed, skipped))
}

async fn load_imposters_from_datadir(
    manager: &Arc<ImposterManager>,
    datadir: &PathBuf,
    allow_injection: bool,
) -> anyhow::Result<()> {
    info!("Loading imposters from datadir: {:?}", datadir);

    if !datadir.exists() {
        std::fs::create_dir_all(datadir)?;
        return Ok(());
    }

    // `file:`/`ref:` scripts in a datadir-loaded imposter resolve relative to the datadir itself,
    // escape-checked: these `{port}.json` files can be network-authored (persisted from an
    // admin-API POST), so an absolute path or `..` escape is rejected, never read (issue #356
    // B1/B2 defense-in-depth against a datadir re-resolution reading `/etc/passwd`).
    let base = ScriptBaseDir::DatadirRelative(datadir.clone());
    let (parsed, mut skipped) = read_and_parse_datadir(datadir, &base)?;
    let (parsed, gated) = partition_gated_datadir(parsed, allow_injection);
    skipped.extend(gated);

    for (path, config) in parsed {
        info!("Loading imposter on port {:?} from {:?}", config.port, path);
        match manager.create_imposter(config).await {
            Ok(port) => info!("Created imposter on port {} from {:?}", port, path),
            // Surfaced once, via the aggregated summary below, uniform with the other skip reasons.
            Err(e) => skipped.push(SkippedImposterFile {
                path,
                reason: format!("imposter creation failed: {e}"),
            }),
        }
    }

    if let Some(summary) = format_skipped_summary(&skipped) {
        error!("{summary}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // The CA load/generate logic now lives behind `InterceptControl::start`; these tests still
    // exercise `CertificateAuthority` directly (its contract is unchanged).
    use rift_mock_core::proxy::intercept_ca::CertificateAuthority;

    #[test]
    fn test_no_parse_flag_accepted() {
        let cli = Cli::try_parse_from(["rift", "--noParse"]).expect("--noParse should be accepted");
        assert!(cli.no_parse);
    }

    // Issue #664: `rift healthcheck` is what the images run as their HEALTHCHECK, so its bare form
    // must parse with no arguments and default to the admin port.
    #[test]
    fn healthcheck_parses_bare() {
        let cli = Cli::try_parse_from(["rift", "healthcheck"]).expect("parse");
        match cli.command {
            Some(Commands::Healthcheck { url, timeout }) => {
                assert!(url.is_none(), "no --url means probe the admin API");
                assert_eq!(timeout, DEFAULT_HEALTHCHECK_TIMEOUT_SECS);
            }
            other => panic!("expected Healthcheck, got {other:?}"),
        }
        assert_eq!(cli.port, DEFAULT_ADMIN_PORT);
    }

    #[test]
    fn healthcheck_accepts_url_and_timeout() {
        let cli = Cli::try_parse_from([
            "rift",
            "healthcheck",
            "--url",
            "http://localhost:9090/metrics",
            "--timeout",
            "10",
        ])
        .expect("parse");
        match cli.command {
            Some(Commands::Healthcheck { url, timeout }) => {
                assert_eq!(url.as_deref(), Some("http://localhost:9090/metrics"));
                assert_eq!(timeout, 10);
            }
            other => panic!("expected Healthcheck, got {other:?}"),
        }
    }

    // The probe has to follow the server's own --port, since that is how the container's MB_PORT
    // reaches it.
    #[test]
    fn healthcheck_follows_the_servers_port_flag() {
        let cli = Cli::try_parse_from(["rift", "--port", "3000", "healthcheck"]).expect("parse");
        assert_eq!(cli.port, 3000);
        assert!(matches!(cli.command, Some(Commands::Healthcheck { .. })));
    }

    // Its budget must stay strictly under the images' `HEALTHCHECK --timeout=3s`, or the probe
    // races Docker's killer instead of reporting the unhealthy verdict itself. A const block makes
    // raising the default past that budget a compile error rather than a test failure.
    #[test]
    fn healthcheck_default_timeout_is_under_the_dockerfile_budget() {
        const DOCKERFILE_HEALTHCHECK_TIMEOUT_SECS: u64 = 3;
        const {
            assert!(
                DEFAULT_HEALTHCHECK_TIMEOUT_SECS < DOCKERFILE_HEALTHCHECK_TIMEOUT_SECS,
                "the probe's budget must be strictly under the Dockerfile HEALTHCHECK --timeout"
            )
        };
    }

    // Issue #532: skipped datadir files must be surfaced, not silently dropped.
    #[test]
    fn format_skipped_summary_empty_is_none() {
        assert!(format_skipped_summary(&[]).is_none());
    }

    #[test]
    fn format_skipped_summary_lists_all() {
        let skipped = vec![
            SkippedImposterFile {
                path: PathBuf::from("/data/4501.json"),
                reason: "invalid imposter JSON: expected value".to_string(),
            },
            SkippedImposterFile {
                path: PathBuf::from("/data/4502.json"),
                reason: "could not read file: permission denied".to_string(),
            },
        ];
        let summary =
            format_skipped_summary(&skipped).expect("non-empty skip list yields a summary");
        assert!(summary.contains('2'), "summary states the count: {summary}");
        assert!(
            summary.contains("NOT being served"),
            "summary warns they are unserved: {summary}"
        );
        assert!(summary.contains("/data/4501.json"));
        assert!(summary.contains("invalid imposter JSON"));
        assert!(summary.contains("/data/4502.json"));
        assert!(summary.contains("could not read file"));
    }

    #[test]
    fn read_and_parse_datadir_collects_malformed_and_keeps_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("4501.json"),
            r#"{"port": 4501, "protocol": "http", "stubs": []}"#,
        )
        .expect("write valid");
        std::fs::write(dir.path().join("4502.json"), "{ this is not valid json ]")
            .expect("write malformed");

        let base = ScriptBaseDir::DatadirRelative(dir.path().to_path_buf());
        let (parsed, skipped) =
            read_and_parse_datadir(dir.path(), &base).expect("read_and_parse must not abort");

        assert_eq!(parsed.len(), 1, "the valid imposter is still parsed");
        assert_eq!(parsed[0].1.port, Some(4501));
        assert_eq!(
            skipped.len(),
            1,
            "the malformed file is collected, not dropped"
        );
        assert!(skipped[0].path.ends_with("4502.json"));
        assert!(skipped[0].reason.contains("invalid imposter JSON"));
    }

    #[test]
    fn read_and_parse_datadir_collects_unresolvable_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A well-formed imposter whose `_rift.script.file` points at a file that isn't in the
        // datadir — resolve_scripts fails, so it must be collected as a skip, not dropped.
        std::fs::write(
            dir.path().join("4503.json"),
            r#"{"port": 4503, "protocol": "http",
                "stubs": [{"responses": [{"_rift": {"script": {"file": "does-not-exist.rhai"}}}]}]}"#,
        )
        .expect("write script-ref imposter");

        let base = ScriptBaseDir::DatadirRelative(dir.path().to_path_buf());
        let (parsed, skipped) = read_and_parse_datadir(dir.path(), &base).expect("read_and_parse");

        assert!(
            parsed.is_empty(),
            "the imposter with an unresolvable script is not parsed"
        );
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].path.ends_with("4503.json"));
        assert!(
            skipped[0].reason.contains("script resolution failed"),
            "reason names the script-resolution failure: {}",
            skipped[0].reason
        );
    }

    #[test]
    fn read_and_parse_datadir_no_skips_when_all_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("4501.json"),
            r#"{"port": 4501, "protocol": "http", "stubs": []}"#,
        )
        .expect("write valid");

        let base = ScriptBaseDir::DatadirRelative(dir.path().to_path_buf());
        let (parsed, skipped) = read_and_parse_datadir(dir.path(), &base).expect("read_and_parse");

        assert_eq!(parsed.len(), 1);
        assert!(skipped.is_empty(), "no summary when every file is valid");
    }

    #[test]
    fn intercept_flags_parse() {
        let cli = Cli::try_parse_from(["rift", "--intercept-port", "9000"]).expect("parse");
        assert_eq!(cli.intercept_port, Some(9000));
        let none = Cli::try_parse_from(["rift"]).expect("parse");
        assert_eq!(none.intercept_port, None);
    }

    // Issue #593 AC5: inline CA PEM flags parse, and conflict with the CA file flags at parse time.
    #[test]
    fn intercept_ca_pem_flags_parse_and_conflict_with_paths() {
        let cli = Cli::try_parse_from([
            "rift",
            "--intercept-ca-cert-pem",
            "CERT",
            "--intercept-ca-key-pem",
            "KEY",
        ])
        .expect("inline PEM flags parse");
        assert_eq!(cli.intercept_ca_cert_pem.as_deref(), Some("CERT"));
        assert_eq!(cli.intercept_ca_key_pem.as_deref(), Some("KEY"));

        // A PEM flag alongside a CA file flag is a hard parse error (D6 conflicts_with).
        assert!(
            Cli::try_parse_from([
                "rift",
                "--intercept-ca-cert",
                "ca.pem",
                "--intercept-ca-cert-pem",
                "CERT",
            ])
            .is_err(),
            "path and inline-PEM CA flags must conflict at parse"
        );
    }

    // Issue #356: `--scripts-dir` is the admin-API `file:` script resolution root.
    #[test]
    fn scripts_dir_flag_parses() {
        let cli = Cli::try_parse_from(["rift", "--scripts-dir", "/tmp/scripts"]).expect("parse");
        assert_eq!(
            cli.scripts_dir,
            Some(std::path::PathBuf::from("/tmp/scripts"))
        );
        let none = Cli::try_parse_from(["rift"]).expect("parse");
        assert_eq!(none.scripts_dir, None);
    }

    #[test]
    fn load_or_generate_ca_rules() {
        use std::path::Path;
        // Neither cert nor key: a CA is generated.
        assert!(CertificateAuthority::load_or_generate(None, None).is_ok());
        // A half-configured pair is rejected rather than silently generating.
        let cert = Path::new("ca.pem");
        assert!(CertificateAuthority::load_or_generate(Some(cert), None).is_err());
        assert!(CertificateAuthority::load_or_generate(None, Some(cert)).is_err());
    }

    #[test]
    fn load_or_generate_ca_loads_supplied_pem() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        use std::io::Write;

        // A real CA cert+key written to disk...
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        let mut cert_file = tempfile::NamedTempFile::new().unwrap();
        let mut key_file = tempfile::NamedTempFile::new().unwrap();
        cert_file.write_all(cert.pem().as_bytes()).unwrap();
        key_file.write_all(key.serialize_pem().as_bytes()).unwrap();

        // ...is loaded, not regenerated: the CA exposes the supplied certificate.
        let ca =
            CertificateAuthority::load_or_generate(Some(cert_file.path()), Some(key_file.path()))
                .expect("load supplied CA");
        assert_eq!(ca.ca_cert_pem(), cert.pem().as_str());
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

    // ===== Issue #612: the --allowInjection gate on the config doors =====
    //
    // A config file and a datadir file must get the same security answer as POST /imposters:
    // a script surface without --allowInjection is refused. Before this, the same document was
    // 400'd by the admin API but loaded and executed via --configfile.

    fn config_from(json: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(json).expect("valid imposter config")
    }

    fn inject_response_config(port: u16) -> ImposterConfig {
        config_from(serde_json::json!({
            "port": port,
            "protocol": "http",
            "stubs": [{"responses": [{"inject": "function (req) { return {body: 'x'}; }"}]}],
        }))
    }

    /// The `examples/latency-testing.json` shape from the issue's Evidence section: that file
    /// spells its scripted wait as the `{"inject": ...}` object form, not a bare string.
    fn js_wait_config(port: u16) -> ImposterConfig {
        config_from(serde_json::json!({
            "port": port,
            "protocol": "http",
            "stubs": [{"responses": [{
                "is": {"statusCode": 200, "body": "ok"},
                "_behaviors": {"wait": {"inject": "function() { return 100; }"}},
            }]}],
        }))
    }

    fn clean_config(port: u16) -> ImposterConfig {
        config_from(serde_json::json!({
            "port": port,
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}],
        }))
    }

    // AC1: an inject response in a configfile aborts startup, and the message must be actionable —
    // it names the file, the offending port, and the flag that would allow it.
    #[test]
    fn configfile_injection_error_names_file_port_and_flag() {
        let path = PathBuf::from("/cfg/imposters.json");
        let err = configfile_injection_error(&path, &[inject_response_config(4545)], false)
            .expect("an inject response without --allowInjection must abort startup");
        assert!(err.contains("/cfg/imposters.json"), "names the file: {err}");
        assert!(err.contains("4545"), "names the offending port: {err}");
        assert!(err.contains("--allowInjection"), "names the flag: {err}");
    }

    // AC2: a JS-function wait is a script surface too — the exact config that the admin API
    // already 400s but --configfile used to execute.
    #[test]
    fn configfile_injection_error_flags_js_function_wait() {
        let path = PathBuf::from("/cfg/latency-testing.json");
        let err = configfile_injection_error(&path, &[js_wait_config(4545)], false)
            .expect("a JS-function wait without --allowInjection must abort startup");
        assert!(err.contains("--allowInjection"), "got: {err}");
    }

    // AC3: the flag is the whole point — with it set, the same config loads.
    #[test]
    fn configfile_injection_error_none_when_flag_set() {
        let path = PathBuf::from("/cfg/imposters.json");
        assert!(
            configfile_injection_error(&path, &[inject_response_config(4545)], true).is_none(),
            "--allowInjection must permit an inject response"
        );
    }

    // AC4: no false positives — a script-free config loads with the flag off.
    #[test]
    fn configfile_injection_error_none_for_clean_config() {
        let path = PathBuf::from("/cfg/imposters.json");
        assert!(
            configfile_injection_error(&path, &[clean_config(4545)], false).is_none(),
            "a config with no script surface must load without --allowInjection"
        );
    }

    // Design requirement: one message listing every offender, so the operator fixes the file in a
    // single pass instead of restarting into the next error.
    #[test]
    fn configfile_injection_error_lists_every_offending_port_at_once() {
        let path = PathBuf::from("/cfg/imposters.json");
        let configs = vec![
            inject_response_config(4545),
            clean_config(4546),
            js_wait_config(4547),
        ];
        let err = configfile_injection_error(&path, &configs, false).expect("offenders present");
        assert!(err.contains("4545"), "lists the first offender: {err}");
        assert!(err.contains("4547"), "lists the second offender: {err}");
        assert!(
            !err.contains("4546"),
            "must not name the clean imposter: {err}"
        );
    }

    // A port-less (auto-assigned) config must still be nameable in the error, not silently omitted.
    #[test]
    fn configfile_injection_error_labels_a_portless_config() {
        let path = PathBuf::from("/cfg/imposters.json");
        let portless = config_from(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"inject": "function (req) { return {}; }"}]}],
        }));
        let err = configfile_injection_error(&path, &[portless], false)
            .expect("a port-less offender must still abort startup");
        assert!(
            err.contains("<auto-assigned>"),
            "a port-less offender must still be labelled, not silently omitted: {err}"
        );
        assert!(err.contains("--allowInjection"), "got: {err}");
    }

    // AC5: a datadir gates per file and fails closed — the leftover scripted file is skipped and
    // named, while the clean file is still served. A persisted `{port}.json` from an earlier
    // --allowInjection run must not brick startup for everything else.
    #[test]
    fn partition_gated_datadir_skips_only_the_scripted_file() {
        let parsed = vec![
            (
                PathBuf::from("/data/4501.json"),
                inject_response_config(4501),
            ),
            (PathBuf::from("/data/4502.json"), clean_config(4502)),
        ];
        let (servable, gated) = partition_gated_datadir(parsed, false);

        assert_eq!(servable.len(), 1, "the clean file stays servable");
        assert_eq!(servable[0].0, PathBuf::from("/data/4502.json"));

        assert_eq!(gated.len(), 1, "the scripted file is skipped");
        assert_eq!(gated[0].path, PathBuf::from("/data/4501.json"));
        assert_eq!(
            gated[0].reason,
            "uses inject/decorate/shellTransform/JS-function wait, which require --allowInjection",
            "the operator-facing skip reason must read cleanly, naming the flag once"
        );

        let summary = format_skipped_summary(&gated).expect("a gated file yields a summary");
        assert!(
            summary.contains("/data/4501.json") && summary.contains("NOT being served"),
            "the gated file flows into the existing skip summary: {summary}"
        );
    }

    // AC6: with the flag set, nothing is gated.
    #[test]
    fn partition_gated_datadir_keeps_everything_when_flag_set() {
        let parsed = vec![
            (
                PathBuf::from("/data/4501.json"),
                inject_response_config(4501),
            ),
            (PathBuf::from("/data/4502.json"), clean_config(4502)),
        ];
        let (servable, gated) = partition_gated_datadir(parsed, true);
        assert_eq!(servable.len(), 2, "--allowInjection serves both");
        assert!(gated.is_empty(), "nothing is gated with the flag set");
    }

    // ===== #612 door-level: the gate must be WIRED, not merely present =====
    //
    // The helper tests above prove the classifier decides correctly. They cannot prove the doors
    // ask it — and #612 was exactly that failure: a call-site that never consulted the gate. These
    // drive the real loaders, so dropping the gate call (or bailing after the create loop) fails
    // here even though every helper test would still pass.

    fn write_json(path: &Path, value: serde_json::Value) {
        std::fs::write(path, serde_json::to_string(&value).expect("json")).expect("write");
    }

    #[tokio::test]
    async fn load_imposters_from_file_aborts_before_creating_any_imposter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        // A clean imposter listed *before* the offender: if the gate ran per-imposter inside the
        // create loop instead of up front, 19601 would already be bound and serving.
        write_json(
            &path,
            serde_json::json!({"imposters": [
                {"port": 19601, "protocol": "http",
                 "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]},
                {"port": 19602, "protocol": "http",
                 "stubs": [{"responses": [{"inject": "function (req) { return {body: 'x'}; }"}]}]},
            ]}),
        );

        let manager = Arc::new(ImposterManager::new());
        let err = load_imposters_from_file(&manager, &path, false, false, &[])
            .await
            .expect_err("a gated configfile must abort startup");
        assert!(err.to_string().contains("--allowInjection"), "got: {err}");

        assert!(
            manager.get_imposter(19601).is_err(),
            "all-or-nothing: the clean imposter must not be half-loaded before the abort"
        );
        assert!(
            manager.get_imposter(19602).is_err(),
            "the offender must not load"
        );

        manager.delete_all().await;
    }

    // The literal repro from the issue's Evidence section: the shipped example is refused by the
    // admin API without --allowInjection, and must now be refused through --configfile too.
    #[tokio::test]
    async fn load_imposters_from_file_refuses_the_shipped_latency_example() {
        let example =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/latency-testing.json");
        assert!(example.exists(), "fixture missing: {}", example.display());

        let manager = Arc::new(ImposterManager::new());
        let err = load_imposters_from_file(&manager, &example, false, false, &[])
            .await
            .expect_err("examples/latency-testing.json uses a JS-function wait; it must be gated");
        assert!(err.to_string().contains("--allowInjection"), "got: {err}");

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn load_imposters_from_datadir_serves_the_clean_file_and_skips_the_scripted_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_json(
            &dir.path().join("19603.json"),
            serde_json::json!({"port": 19603, "protocol": "http",
                "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]}),
        );
        write_json(
            &dir.path().join("19604.json"),
            serde_json::json!({"port": 19604, "protocol": "http",
                "stubs": [{"responses": [{"inject": "function (req) { return {body: 'x'}; }"}]}]}),
        );

        let manager = Arc::new(ImposterManager::new());
        let datadir = dir.path().to_path_buf();
        load_imposters_from_datadir(&manager, &datadir, false)
            .await
            .expect("a gated datadir file is skipped, never fatal");

        assert!(
            manager.get_imposter(19603).is_ok(),
            "the clean file must still be served — one gated file cannot brick startup"
        );
        assert!(
            manager.get_imposter(19604).is_err(),
            "the scripted file must not be served without --allowInjection"
        );

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn load_imposters_from_datadir_serves_the_scripted_file_with_the_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_json(
            &dir.path().join("19605.json"),
            serde_json::json!({"port": 19605, "protocol": "http",
                "stubs": [{"responses": [{"inject": "function (req) { return {body: 'x'}; }"}]}]}),
        );

        let manager = Arc::new(ImposterManager::new());
        let datadir = dir.path().to_path_buf();
        load_imposters_from_datadir(&manager, &datadir, true)
            .await
            .expect("datadir load succeeds");
        assert!(
            manager.get_imposter(19605).is_ok(),
            "--allowInjection must serve the scripted file"
        );

        manager.delete_all().await;
    }

    // ===== `intercept` config block: source conflict + injection gate (issue #655) =====

    fn rule_from(value: serde_json::Value) -> crate::intercept_rules::InterceptRule {
        serde_json::from_value(value).expect("valid intercept rule")
    }

    fn clean_rule() -> crate::intercept_rules::InterceptRule {
        rule_from(serde_json::json!({
            "host": "cdn.example.com",
            "predicates": [{"equals": {"path": "/config.json"}}],
            "action": {"forward": {"port": 4545}}
        }))
    }

    fn inject_rule() -> crate::intercept_rules::InterceptRule {
        rule_from(serde_json::json!({
            "host": "evil.example.com",
            "predicates": [{"inject": "function (req) { return true; }"}],
            "action": {"serve": {"statusCode": 200}}
        }))
    }

    /// AC3: the block and `--intercept-port` are two spellings of one listener. Supplying both is a
    /// startup error naming the offending flag, not a silent precedence guess.
    #[test]
    fn intercept_conflict_names_every_supplied_flag() {
        for (args, expected) in [
            (vec!["rift", "--intercept-port", "8080"], "--intercept-port"),
            (
                vec![
                    "rift",
                    "--intercept-ca-cert",
                    "c.pem",
                    "--intercept-ca-key",
                    "k.pem",
                ],
                "--intercept-ca-cert",
            ),
            (
                vec![
                    "rift",
                    "--intercept-ca-cert-pem",
                    "PEM",
                    "--intercept-ca-key-pem",
                    "PEM",
                ],
                "--intercept-ca-cert-pem",
            ),
        ] {
            let cli = Cli::parse_from(args);
            let flags = cli_intercept_flags(&cli);
            let err = intercept_source_conflict_error(Path::new("/cfg/x.json"), &flags)
                .expect("block + flag must conflict");
            assert!(err.contains(expected), "names the flag: {err}");
            assert!(err.contains("/cfg/x.json"), "names the file: {err}");
        }
    }

    /// AC3 (negative): neither source alone conflicts — the block is additive, the flags keep working.
    #[test]
    fn intercept_conflict_absent_when_only_one_source() {
        let cli = Cli::parse_from(["rift"]);
        assert!(
            cli_intercept_flags(&cli).is_empty(),
            "no flags supplied means no conflict with a block"
        );
        assert!(
            intercept_source_conflict_error(Path::new("/cfg/x.json"), &[]).is_none(),
            "a block with no flags is the whole point of the feature"
        );
    }

    /// AC4: an `inject` predicate arriving by config file is executable code crossing the same
    /// trust boundary the imposter gate already guards — it must be refused without the flag.
    #[test]
    fn configfile_intercept_injection_error_names_file_rule_and_flag() {
        let err = configfile_intercept_injection_error(
            Path::new("/cfg/optimizely.json"),
            &[clean_rule(), inject_rule()],
            false,
        )
        .expect("an inject predicate without --allowInjection must abort startup");
        assert!(
            err.contains("/cfg/optimizely.json"),
            "names the file: {err}"
        );
        assert!(err.contains("evil.example.com"), "names the rule: {err}");
        assert!(err.contains("--allowInjection"), "names the flag: {err}");
        assert!(
            !err.contains("cdn.example.com"),
            "must not name the clean rule: {err}"
        );
    }

    /// The gate must see through `not`/`or`/`and` nesting, exactly as it does for imposter stubs —
    /// otherwise wrapping the inject in a `not` walks straight past it.
    #[test]
    fn configfile_intercept_injection_error_sees_nested_inject() {
        let nested = rule_from(serde_json::json!({
            "host": "nested.example.com",
            "predicates": [{"or": [
                {"equals": {"path": "/a"}},
                {"not": {"inject": "function (req) { return true; }"}}
            ]}],
            "action": {"serve": {"statusCode": 200}}
        }));
        assert!(
            configfile_intercept_injection_error(Path::new("/cfg/x.json"), &[nested], false)
                .is_some(),
            "an inject nested under or/not must still be gated"
        );
    }

    /// AC4 (negatives): the flag admits it, and a script-free rule set never trips the gate.
    #[test]
    fn configfile_intercept_injection_error_none_when_allowed_or_clean() {
        assert!(
            configfile_intercept_injection_error(Path::new("/cfg/x.json"), &[inject_rule()], true)
                .is_none(),
            "--allowInjection must permit an inject predicate"
        );
        assert!(
            configfile_intercept_injection_error(Path::new("/cfg/x.json"), &[clean_rule()], false)
                .is_none(),
            "a rule with no script surface must load without --allowInjection"
        );
        assert!(
            configfile_intercept_injection_error(Path::new("/cfg/x.json"), &[], false).is_none(),
            "an empty rule set is admissible"
        );
    }

    /// A `serve` action is a fixed stub and `forward` carries no script, so neither is a gated
    /// surface — pinning that the gate does not over-reach into refusing ordinary rules.
    #[test]
    fn serve_and_forward_actions_are_not_script_surfaces() {
        let serve = rule_from(serde_json::json!({
            "action": {"serve": {"statusCode": 200, "body": "function (req) { return true; }"}}
        }));
        assert!(
            configfile_intercept_injection_error(Path::new("/cfg/x.json"), &[serve], false)
                .is_none(),
            "a serve body that merely looks like JS is inert data, not an injection"
        );
    }
}
