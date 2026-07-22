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
///
/// Idempotent about the end state, loud about everything else: a stale pidfile (the process is
/// already gone) is cleaned up and reported `Ok`, but a signal that is denied (the process is not
/// ours) or fails unexpectedly is an error and the pidfile is left in place — it is not stale.
///
/// The unix arm inspects `kill`'s errno to make that distinction; the Windows arm only checks
/// whether `taskkill` succeeded (it does not map "no such process" back onto the stale-pidfile
/// policy — that exit code is undocumented-ish and untested here).
pub fn stop_server(pidfile: &Path) -> Result<(), anyhow::Error> {
    if !pidfile.exists() {
        return Err(anyhow::anyhow!("PID file not found: {pidfile:?}"));
    }

    let pid_str = std::fs::read_to_string(pidfile)?;
    let pid: i32 = pid_str.trim().parse()?;

    info!("Stopping server with PID {}", pid);

    #[cfg(unix)]
    {
        // SAFETY: kill(2) with a plain PID and signal number touches no memory; failure is
        // reported via errno, which we read immediately below.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ESRCH) => {
                    // No such process — the pidfile is stale. The desired end state already holds.
                    warn!("process {pid} not running; removing stale PID file");
                }
                Some(libc::EPERM) => {
                    return Err(anyhow::anyhow!(
                        "not permitted to signal process {pid} (EPERM); leaving PID file in place"
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "failed to signal process {pid}: {err}; leaving PID file in place"
                    ));
                }
            }
        }
    }

    #[cfg(windows)]
    {
        // On Windows, use taskkill; a non-success exit means the process was not stopped, so the
        // pidfile is not stale — surface the failure and keep it.
        let output = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "taskkill failed to stop process {pid}: {}; leaving PID file in place",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    // Remove PID file (success path, and the ESRCH stale-pidfile path).
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

    // `error_for_status` before `.text()` so a 401/500 response is a value error, not a body
    // silently written to the user's savefile. The error carries the status and URL.
    let response = client.get(&url).send().await?.error_for_status()?;
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
