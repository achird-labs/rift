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
//!
//! The server composition itself (CLI surface, bootstrap, metrics, gateway dispatch) lives
//! in the `rift_http_proxy` library (issue #317); this binary is a thin caller.

// Route the server binary's allocations through mimalloc (issue #293). Gated by
// the default-on `mimalloc` feature so FFI/cross-compile builds can drop it; the
// allocator is set only here in the binary, never in the rift-core/rift-ffi libs.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use rift_http_proxy::admin_api::DEFAULT_ADMIN_PORT;
use rift_http_proxy::server::{Cli, Commands, ServerBuilder};
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, Layer, fmt, prelude::*};

fn main() -> Result<(), anyhow::Error> {
    let mut cli = Cli::parse();

    // Apply rcfile defaults before using CLI values (only for fields at their clap defaults)
    if let Some(ref rcfile) = cli.rcfile.clone() {
        match apply_rcfile_defaults(&mut cli, rcfile) {
            Ok(()) => {}
            Err(e) => eprintln!("Warning: failed to load --rcfile {rcfile:?}: {e}"),
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

    runtime.block_on(ServerBuilder::from_cli(cli).run())
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
                if cli.port == DEFAULT_ADMIN_PORT
                    && let Some(p) = val.as_u64()
                {
                    cli.port = p as u16;
                }
            }
            "host" => {
                if cli.host == "0.0.0.0"
                    && let Some(h) = val.as_str()
                {
                    cli.host = h.to_string();
                }
            }
            "logLevel" | "loglevel" => {
                if cli.loglevel == "info"
                    && let Some(l) = val.as_str()
                {
                    cli.loglevel = l.to_string();
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
                if cli.datadir.is_none()
                    && let Some(d) = val.as_str()
                {
                    cli.datadir = Some(std::path::PathBuf::from(d));
                }
            }
            "configfile" => {
                if cli.configfile.is_none()
                    && let Some(f) = val.as_str()
                {
                    cli.configfile = Some(std::path::PathBuf::from(f));
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
