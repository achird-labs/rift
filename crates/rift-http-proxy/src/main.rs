// Allow dead_code for test targets - functions are used at runtime but not in tests
#![allow(dead_code)]

//! Rift HTTP Proxy - A Mountebank-compatible chaos engineering proxy
//!
//! Rift provides a Mountebank-compatible API with advanced features like:
//! - Probabilistic fault injection via `_rift.fault` extensions
//! - Multi-engine scripting (Rhai, Lua, JavaScript) via `_rift.script`
//! - Stateful testing with flow store via `_rift.flowState`
//!
//! # Examples
//!
//! Start Rift server:
//! ```bash
//! rift                                    # Admin API on port 2525
//! rift --port 3000                        # Admin API on port 3000
//! rift --configfile imposters.json        # Load imposters from file
//! rift --datadir ./mb-data                # Persist imposters to directory
//! ```

// ===== Core Mountebank-compatible modules =====
mod admin_api;
mod backends;
mod behaviors;
mod config;
mod imposter;
mod predicate;
mod proxy;
mod recording;

// ===== Rift Extensions (features beyond Mountebank) =====
mod extensions;
mod response;

// Shared utilities
mod util;

// Internal modules
mod scripting;

// Re-export extension modules for convenience
use extensions::metrics;

use admin_api::AdminApiServer;
use clap::{Parser, Subcommand};
use imposter::{ImposterConfig, ImposterManager};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Rift - A Mountebank-compatible HTTP chaos engineering proxy
///
/// Rift starts an admin API on port 2525 (configurable) for creating imposters
/// with advanced fault injection, scripting, and stateful testing capabilities.
#[derive(Parser, Debug)]
#[command(name = "rift")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    // === Mountebank-compatible options ===
    /// Port for the admin API (Mountebank mode)
    #[arg(long, default_value = "2525", env = "MB_PORT")]
    port: u16,

    /// Hostname to bind the admin API to
    #[arg(long, default_value = "0.0.0.0", env = "MB_HOST")]
    host: String,

    /// Load imposters from a config file on startup (JSON or EJS format)
    #[arg(long, value_name = "FILE", env = "MB_CONFIGFILE")]
    configfile: Option<PathBuf>,

    /// Directory for persistent imposter storage
    #[arg(long, value_name = "DIR", env = "MB_DATADIR")]
    datadir: Option<PathBuf>,

    /// Allow JavaScript injection in responses (for inject and decorate)
    #[arg(long, visible_alias = "allowInjection", env = "MB_ALLOW_INJECTION")]
    allow_injection: bool,

    /// Only accept requests from localhost
    #[arg(long, env = "MB_LOCAL_ONLY")]
    local_only: bool,

    /// Log level (debug, info, warn, error)
    #[arg(long, default_value = "info", env = "MB_LOGLEVEL")]
    loglevel: String,

    /// Don't write to log file (stdout only)
    #[arg(long)]
    nologfile: bool,

    /// Log file path (default: mb.log in current directory)
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,

    /// PID file path
    #[arg(long, value_name = "FILE")]
    pidfile: Option<PathBuf>,

    /// CORS allowed origin
    #[arg(long)]
    origin: Option<String>,

    /// IP addresses allowed to connect (comma-separated)
    #[arg(long, value_delimiter = ',')]
    ip_whitelist: Option<Vec<String>>,

    /// Run in mock mode (all imposters are mocks)
    #[arg(long)]
    mock: bool,

    /// Enable debug mode
    #[arg(long)]
    debug: bool,

    /// Metrics server port
    #[arg(long, default_value = "9090", env = "RIFT_METRICS_PORT")]
    metrics_port: u16,

    // === Mountebank compatibility flags (accepted, no-op) ===
    /// Disable EJS template rendering of --configfile (Rift doesn't use EJS; accepted for compatibility)
    #[arg(long, visible_alias = "noParse")]
    no_parse: bool,

    /// Custom config formatter module name (Rift auto-detects JSON/YAML; accepted for compatibility)
    #[arg(long)]
    formatter: Option<String>,

    /// Custom protocol definitions file (custom protocols not yet supported; accepted for compatibility)
    #[arg(long, value_name = "FILE")]
    protofile: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
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
        /// Output file path
        #[arg(long, required = true)]
        savefile: PathBuf,

        /// Include recorded requests in output
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

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    // Install default cryptographic provider for rustls
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install default crypto provider"))?;

    // Initialize tracing based on loglevel
    let log_level = match cli.loglevel.to_lowercase().as_str() {
        "debug" => "debug",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "info",
    };

    let filter = if cli.debug { "debug" } else { log_level };

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)))
        .init();

    // Write PID file if requested
    if let Some(ref pidfile) = cli.pidfile {
        let pid = std::process::id();
        std::fs::write(pidfile, pid.to_string())?;
        info!("Wrote PID {} to {:?}", pid, pidfile);
    }

    // Handle subcommands
    match &cli.command {
        Some(Commands::Stop { pidfile }) => {
            return stop_server(pidfile);
        }
        Some(Commands::Restart { pidfile }) => {
            stop_server(pidfile)?;
            // Fall through to start
        }
        Some(Commands::Save { savefile, .. }) => {
            return save_imposters(&cli, savefile);
        }
        Some(Commands::Replay { configfile }) => {
            // Load the config file and start
            return run_mountebank_mode(Cli {
                configfile: Some(configfile.clone()),
                ..cli
            });
        }
        Some(Commands::Start) | None => {
            // Default behavior - start in Mountebank mode
        }
    }

    // Start in Mountebank mode
    info!("Starting Rift on port {}", cli.port);
    run_mountebank_mode(cli)
}

/// Run in Mountebank-compatible mode
fn run_mountebank_mode(cli: Cli) -> Result<(), anyhow::Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        // Create imposter manager
        let manager = Arc::new(ImposterManager::new());

        // Load imposters from configfile if provided
        if let Some(ref configfile) = cli.configfile {
            load_imposters_from_file(&manager, configfile).await?;
        }

        // Load imposters from datadir if provided
        if let Some(ref datadir) = cli.datadir {
            load_imposters_from_datadir(&manager, datadir).await?;
        }

        // Start metrics server
        let metrics_port = cli.metrics_port;
        tokio::spawn(async move {
            if let Err(e) = run_metrics_server(metrics_port).await {
                error!("Metrics server error: {}", e);
            }
        });

        // Determine bind address
        let host = if cli.local_only {
            "127.0.0.1"
        } else {
            &cli.host
        };

        let addr: SocketAddr = format!("{}:{}", host, cli.port).parse()?;

        // Start admin API server
        info!(
            "Rift Admin API (Mountebank-compatible) starting on http://{}",
            addr
        );
        info!(
            "Metrics available at http://{}:{}/metrics",
            host, metrics_port
        );

        if cli.allow_injection {
            info!("JavaScript injection enabled");
        }

        if cli.formatter.is_some() {
            tracing::warn!(
                "--formatter is not supported; Rift auto-detects JSON/YAML config formats"
            );
        }
        if cli.protofile.is_some() {
            tracing::warn!(
                "--protofile is not supported; custom protocols are not yet implemented"
            );
        }

        let server = AdminApiServer::new(addr, manager);
        server.run().await?;

        Ok(())
    })
}

/// Load imposters from a JSON config file
async fn load_imposters_from_file(
    manager: &Arc<ImposterManager>,
    path: &PathBuf,
) -> Result<(), anyhow::Error> {
    info!("Loading imposters from configfile: {:?}", path);

    let content = std::fs::read_to_string(path)?;

    // Try to parse as JSON (Mountebank format)
    let imposters: Vec<ImposterConfig> = if content.trim().starts_with('{') {
        // Single imposter or wrapper object
        let value: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(imposters) = value.get("imposters") {
            serde_json::from_value(imposters.clone())?
        } else {
            // Single imposter
            vec![serde_json::from_value(value)?]
        }
    } else if content.trim().starts_with('[') {
        // Array of imposters
        serde_json::from_str(&content)?
    } else {
        // Try YAML
        serde_yaml::from_str(&content)?
    };

    for config in imposters {
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
) -> Result<(), anyhow::Error> {
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

/// Stop a running server by PID file
fn stop_server(pidfile: &PathBuf) -> Result<(), anyhow::Error> {
    if !pidfile.exists() {
        return Err(anyhow::anyhow!("PID file not found: {pidfile:?}"));
    }

    let pid_str = std::fs::read_to_string(pidfile)?;
    let pid: i32 = pid_str.trim().parse()?;

    info!("Stopping server with PID {}", pid);

    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    #[cfg(windows)]
    {
        // On Windows, use taskkill
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()?;
    }

    // Remove PID file
    std::fs::remove_file(pidfile)?;

    Ok(())
}

/// Save imposters to a file
fn save_imposters(cli: &Cli, savefile: &PathBuf) -> Result<(), anyhow::Error> {
    let runtime = tokio::runtime::Runtime::new()?;

    runtime.block_on(async {
        let client = reqwest::Client::new();
        let url = format!("http://{}:{}/imposters?replayable=true", cli.host, cli.port);

        let response = client.get(&url).send().await?;
        let content = response.text().await?;

        std::fs::write(savefile, &content)?;
        info!("Saved imposters to {:?}", savefile);

        Ok(())
    })
}

/// Run the metrics server
async fn run_metrics_server(port: u16) -> anyhow::Result<()> {
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{body::Incoming, Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::net::TcpListener;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("Metrics server listening on http://{}/metrics", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| async move {
                if req.uri().path() == "/metrics" {
                    let metrics = metrics::collect_metrics();
                    Ok::<_, Infallible>(Response::new(metrics))
                } else {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(404)
                            .body("Not Found\n".to_string())
                            .unwrap(),
                    )
                }
            });

            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                error!("Metrics server connection error: {}", err);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
}
