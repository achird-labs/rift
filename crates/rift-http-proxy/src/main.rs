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

// The CLI-free engine lives in `rift-core` (issue #203). Bring the modules this binary
// references into its crate root so the existing `crate::<module>` paths resolve unchanged.
use rift_core::{extensions, imposter, scripting};

// ===== Admin HTTP server (control plane — server crate only) =====
mod admin_api;

// Re-export extension modules for convenience
use extensions::metrics;

use admin_api::AdminApiServer;
use clap::{Parser, Subcommand};
use imposter::{ImposterConfig, ImposterManager};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Layer};

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

    /// Require this token in the Authorization header for all admin API requests
    #[arg(long, value_name = "TOKEN", env = "MB_APIKEY")]
    api_key: Option<String>,

    /// RC file with default flag values (Mountebank compatibility; partial support — port/host/loglevel only)
    #[arg(long, value_name = "FILE")]
    rcfile: Option<PathBuf>,
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

fn main() -> Result<(), anyhow::Error> {
    let mut cli = Cli::parse();

    // Apply rcfile defaults before using CLI values (only for fields at their clap defaults)
    if let Some(ref rcfile) = cli.rcfile.clone() {
        match apply_rcfile_defaults(&mut cli, rcfile) {
            Ok(()) => {}
            Err(e) => eprintln!("Warning: failed to load --rcfile {:?}: {}", rcfile, e),
        }
    }

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
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // Build optional file log layer when --log is set and --nologfile is not
    let file_layer: Option<Box<dyn Layer<_> + Send + Sync>> = if !cli.nologfile {
        cli.log.as_ref().and_then(|log_path| {
            let dir = log_path.parent().unwrap_or(std::path::Path::new("."));
            let filename = log_path.file_name()?.to_string_lossy().into_owned();
            let file_appender = tracing_appender::rolling::never(dir, filename);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            // Leak the guard so it lives for the process lifetime
            Box::leak(Box::new(guard));
            Some(fmt::layer().with_writer(non_blocking).boxed())
        })
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .with(file_layer)
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
        Some(Commands::Save {
            savefile,
            remove_proxies,
        }) => {
            return save_imposters(&cli, savefile, *remove_proxies);
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
        // Create imposter manager (with write-through if --datadir is set)
        let manager = Arc::new(ImposterManager::with_datadir(cli.datadir.clone()));

        // Load imposters from configfile if provided
        if let Some(ref configfile) = cli.configfile {
            load_imposters_from_file(&manager, configfile, cli.no_parse).await?;
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

        let server = AdminApiServer::new(addr, manager, cli.api_key);
        server.run().await?;

        Ok(())
    })
}

/// Load imposters from a JSON config file
async fn load_imposters_from_file(
    manager: &Arc<ImposterManager>,
    path: &PathBuf,
    no_parse: bool,
) -> Result<(), anyhow::Error> {
    info!("Loading imposters from configfile: {:?}", path);

    let raw = std::fs::read_to_string(path)?;
    let content = if no_parse {
        raw
    } else {
        preprocess_ejs(&raw, path)?
    };

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

/// Pre-process EJS tokens in a config file before JSON/YAML parsing.
///
/// Handles the patterns emitted by Mountebank and compatible tooling:
/// - `<% include 'path' %>` — inline the referenced file (relative to the config file)
/// - `<%= process.env.VAR %>` — substitute with the env var value (empty string if unset)
/// - `<%= process.env.VAR || 'default' %>` — substitute with env var or the literal default
///
/// Any other `<%= expr %>` token is replaced with an empty string and logged as a warning.
/// `<% expr %>` (without `=`) statements (e.g., `<% for (...) %>`) are removed and logged.
fn preprocess_ejs(content: &str, config_path: &std::path::Path) -> Result<String, anyhow::Error> {
    if !content.contains("<%") {
        return Ok(content.to_string());
    }

    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    // Process include directives first:
    // `<% include 'path' %>`, `<% include "path" %>`, or `<% include path %>`
    let include_re = regex::Regex::new(r#"<%\s*include\s+['"]?([^'">\s]+)['"]?\s*%>"#).unwrap();
    let mut result = String::new();
    let mut last = 0;
    for cap in include_re.captures_iter(content) {
        let full = cap.get(0).unwrap();
        let include_path = cap.get(1).unwrap().as_str();
        result.push_str(&content[last..full.start()]);
        let abs_path = config_dir.join(include_path);
        match std::fs::read_to_string(&abs_path) {
            Ok(included) => result.push_str(&included),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "EJS include file '{}' not found ({}): {}",
                    include_path,
                    abs_path.display(),
                    e
                ));
            }
        }
        last = full.end();
    }
    result.push_str(&content[last..]);
    let content = result;

    // Process expression tags: `<%= expr %>`
    let expr_re = regex::Regex::new(r"<%=\s*(.*?)\s*%>").unwrap();
    let env_var_re = regex::Regex::new(
        r#"^process\.env\.([A-Za-z_][A-Za-z0-9_]*)(?:\s*\|\|\s*['"]([^'"]*)['"]\s*)?$"#,
    )
    .unwrap();

    let mut result = String::new();
    let mut last = 0;
    for cap in expr_re.captures_iter(&content) {
        let full = cap.get(0).unwrap();
        let expr = cap.get(1).unwrap().as_str().trim();
        result.push_str(&content[last..full.start()]);

        if let Some(env_cap) = env_var_re.captures(expr) {
            let var_name = env_cap.get(1).unwrap().as_str();
            let default_val = env_cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let value = std::env::var(var_name).unwrap_or_else(|_| default_val.to_string());
            result.push_str(&value);
        } else {
            warn!(
                "EJS expression '{}' is not supported; substituting empty string",
                expr
            );
        }
        last = full.end();
    }
    result.push_str(&content[last..]);
    let content = result;

    // Strip remaining `<% ... %>` control blocks (non-expression tags); (?s) enables dotall
    let stmt_re = regex::Regex::new(r"(?s)<%[^=].*?%>").unwrap();
    if stmt_re.is_match(&content) {
        warn!("EJS statement blocks (<% ... %>) are not supported and will be removed");
    }
    Ok(stmt_re.replace_all(&content, "").to_string())
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

/// Apply defaults from a Mountebank-compatible rcfile (JSON) to the CLI struct.
/// Only sets fields that are still at their clap defaults (i.e., not explicitly supplied
/// on the command line). Only a subset of keys is supported; unrecognised keys are warned.
fn apply_rcfile_defaults(cli: &mut Cli, rcfile: &std::path::Path) -> Result<(), anyhow::Error> {
    let raw = std::fs::read_to_string(rcfile)?;
    let obj: serde_json::Value = serde_json::from_str(&raw)?;
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("rcfile must be a JSON object"))?;

    for (key, val) in map {
        match key.as_str() {
            "port" => {
                if cli.port == 2525 {
                    if let Some(p) = val.as_u64() {
                        cli.port = p as u16;
                    }
                }
            }
            "host" => {
                if cli.host == "0.0.0.0" {
                    if let Some(h) = val.as_str() {
                        cli.host = h.to_string();
                    }
                }
            }
            "logLevel" | "loglevel" => {
                if cli.loglevel == "info" {
                    if let Some(l) = val.as_str() {
                        cli.loglevel = l.to_string();
                    }
                }
            }
            "allowInjection" | "allow_injection" => {
                if !cli.allow_injection {
                    cli.allow_injection = val.as_bool().unwrap_or(false);
                }
            }
            "localOnly" | "local_only" => {
                if !cli.local_only {
                    cli.local_only = val.as_bool().unwrap_or(false);
                }
            }
            "datadir" => {
                if cli.datadir.is_none() {
                    if let Some(d) = val.as_str() {
                        cli.datadir = Some(std::path::PathBuf::from(d));
                    }
                }
            }
            "configfile" => {
                if cli.configfile.is_none() {
                    if let Some(f) = val.as_str() {
                        cli.configfile = Some(std::path::PathBuf::from(f));
                    }
                }
            }
            other => {
                warn!("--rcfile: unsupported key '{}' (ignored)", other);
            }
        }
    }
    Ok(())
}

/// Save imposters to a file
fn save_imposters(
    cli: &Cli,
    savefile: &PathBuf,
    remove_proxies: bool,
) -> Result<(), anyhow::Error> {
    let runtime = tokio::runtime::Runtime::new()?;

    runtime.block_on(async {
        let client = reqwest::Client::new();
        let mut query = "replayable=true".to_string();
        if remove_proxies {
            query.push_str("&removeProxies=true");
        }
        let url = format!("http://{}:{}/imposters?{}", cli.host, cli.port, query);

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

    // =========================================================================
    // Gap 8.1: EJS configfile pre-processing
    // =========================================================================

    #[test]
    fn test_ejs_no_tokens_passthrough() {
        let content = r#"{"imposters": []}"#;
        let path = std::path::PathBuf::from("config.json");
        assert_eq!(preprocess_ejs(content, &path).unwrap(), content);
    }

    #[test]
    fn test_ejs_env_var_substitution() {
        std::env::set_var("RIFT_TEST_HOST", "myhost");
        let content = r#"{"body": "<%= process.env.RIFT_TEST_HOST %>"}"#;
        let path = std::path::PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"body": "myhost"}"#);
        std::env::remove_var("RIFT_TEST_HOST");
    }

    #[test]
    fn test_ejs_env_var_with_default() {
        std::env::remove_var("RIFT_TEST_UNSET_VAR");
        let content = r#"{"port": "<%= process.env.RIFT_TEST_UNSET_VAR || '4545' %>"}"#;
        let path = std::path::PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "4545"}"#);
    }

    #[test]
    fn test_ejs_env_var_present_overrides_default() {
        std::env::set_var("RIFT_TEST_PORT", "8080");
        let content = r#"{"port": "<%= process.env.RIFT_TEST_PORT || '4545' %>"}"#;
        let path = std::path::PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "8080"}"#);
        std::env::remove_var("RIFT_TEST_PORT");
    }

    #[test]
    fn test_ejs_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let partial_path = dir.path().join("partial.json");
        std::fs::write(&partial_path, r#"{"key": "value"}"#).unwrap();

        let content = r#"<% include 'partial.json' %>"#.to_string();
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(&content, &config_path).unwrap();
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_ejs_include_unquoted_path() {
        let dir = tempfile::tempdir().unwrap();
        let partial_path = dir.path().join("partial.json");
        std::fs::write(&partial_path, r#"[1,2,3]"#).unwrap();

        let content = r#"<% include partial.json %>"#;
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(content, &config_path).unwrap();
        assert_eq!(result, "[1,2,3]");
    }

    #[test]
    fn test_ejs_missing_include_is_fatal_error() {
        let content = r#"<% include 'nonexistent.json' %>"#;
        let path = std::path::PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path);
        assert!(result.is_err(), "missing include file should return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent.json"),
            "error message should name the missing file"
        );
    }
}
