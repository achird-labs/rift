//! Bootstrap concerns shared between the `rift` binary and alternative binaries (issue #807).
//!
//! `--rcfile` defaults, `stop`/`restart` PID-file handling, and `--save` were originally private
//! functions in the `rift` binary's `main.rs`. That made them unreachable from an alternative
//! binary composed on top of this crate (e.g. rift-enterprise's `rift-ee-server`), which could
//! only get the same behaviour by copy-pasting the functions — a fork of behaviour that is meant
//! to stay identical across binaries. Promoting them here, unchanged, gives every binary a single
//! shared implementation instead.

use crate::admin_api::DEFAULT_ADMIN_PORT;
use crate::server::Cli;
use std::path::Path;
use tracing::{info, warn};

/// Apply defaults from a Mountebank-compatible rcfile (JSON) to the CLI struct.
///
/// Only sets fields that are still at their clap defaults (i.e., not explicitly supplied
/// on the command line). Only a subset of keys is supported; unrecognised keys are warned.
pub fn apply_rcfile_defaults(cli: &mut Cli, rcfile: &Path) -> Result<(), anyhow::Error> {
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

/// Stop a running server by PID file.
pub fn stop_server(pidfile: &Path) -> Result<(), anyhow::Error> {
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

/// Save imposters to a file (async form).
///
/// Fetches the replayable imposter config from the admin API at `host:port` and writes it to
/// `savefile`. This is the form to call from an embedder's own async runtime — it awaits rather
/// than driving a nested runtime, so it is safe on an async worker thread. Sync callers (the `save`
/// subcommand) should use [`save_imposters`], which wraps this.
pub async fn save_imposters_async(
    host: &str,
    port: u16,
    savefile: &Path,
    remove_proxies: bool,
) -> Result<(), anyhow::Error> {
    let client = reqwest::Client::new();
    let mut query = "replayable=true".to_string();
    if remove_proxies {
        query.push_str("&removeProxies=true");
    }
    let url = format!("http://{host}:{port}/imposters?{query}");

    let response = client.get(&url).send().await?;
    let content = response.text().await?;

    // `tokio::fs::write` so the shared body never blocks a caller's async worker thread.
    tokio::fs::write(savefile, &content).await?;
    info!("Saved imposters to {:?}", savefile);

    Ok(())
}

/// Save imposters to a file (blocking form).
///
/// Builds its own tokio runtime and drives [`save_imposters_async`], exactly like the CLI's `save`
/// subcommand does today — so this must **not** be called from inside an already-running async
/// runtime (it will panic trying to start a nested one). Call it from sync context; from async
/// code call [`save_imposters_async`] directly.
pub fn save_imposters(
    host: &str,
    port: u16,
    savefile: &Path,
    remove_proxies: bool,
) -> Result<(), anyhow::Error> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(save_imposters_async(host, port, savefile, remove_proxies))
}
