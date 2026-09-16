//! Bootstrap concerns shared between the `rift` binary and alternative binaries (issue #807).
//!
//! `--rcfile` defaults, `stop`/`restart` PID-file handling, and `--save` were originally private
//! functions in the `rift` binary's `main.rs`. That made them unreachable from an alternative
//! binary composed on top of this crate (e.g. rift-cluster's `rift-cluster-server`), which could
//! only get the same behaviour by copy-pasting the functions — a fork of behaviour that is meant
//! to stay identical across binaries. Promoting them here, unchanged, gives every binary a single
//! shared implementation instead.

use crate::admin_api::DEFAULT_ADMIN_PORT;
use crate::server::Cli;
use anyhow::Context;
use std::path::Path;
use tracing::{info, warn};

/// Apply defaults from a Mountebank-compatible rcfile (JSON) to the CLI struct.
///
/// Only sets fields that are still at their clap defaults (i.e., not explicitly supplied
/// on the command line). Only a subset of keys is supported; unrecognised keys are logged with
/// `warn!` — see [`apply_rcfile_defaults_reporting`] to get them back instead.
pub fn apply_rcfile_defaults(cli: &mut Cli, rcfile: &Path) -> Result<(), anyhow::Error> {
    for key in apply_rcfile_defaults_reporting(cli, rcfile)? {
        warn!("--rcfile: unsupported key '{}' (ignored)", key);
    }
    Ok(())
}

/// [`apply_rcfile_defaults`], returning the unsupported keys (in key order) instead of logging them,
/// for a caller that has no log subscriber yet — the `rift` binary applies the rcfile before it
/// installs one, because the rcfile can set the log level (issue #1114).
///
/// # Errors
///
/// The file cannot be read or parsed, is not a JSON object, or gives a recognised key a value of the
/// wrong type (a non-boolean flag, a non-string path or host, a `port` that is not an integer from 0
/// to 65535). Nothing is applied then.
pub fn apply_rcfile_defaults_reporting(
    cli: &mut Cli,
    rcfile: &Path,
) -> Result<Vec<String>, anyhow::Error> {
    // Every failure path out of this function names the file (issue #946). `--rcfile` is resolved
    // from more than one place and a fleet can carry several, and neither `std::fs` nor
    // `serde_json` puts the path in its own error — so an embedder calling this seam directly
    // (the reason #807 made it public) otherwise gets "No such file or directory", or a line and
    // column in a document that is never identified.
    let raw = std::fs::read_to_string(rcfile)
        .with_context(|| format!("reading rcfile {}", rcfile.display()))?;
    let obj: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing rcfile {}", rcfile.display()))?;
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("rcfile {} must be a JSON object", rcfile.display()))?;

    // Type errors are found before any key is applied, so a refused rcfile changes nothing. Every
    // recognised key is checked (issue #1114): a wrong-typed value used to be ignored or coerced, so
    // `"localOnly": "yes"` bound the admin plane on every interface and `"port": 70000` bound 4464,
    // with nothing said. Applying keys until the first bad one also left the rest unset.
    for (key, val) in map {
        let Some(expected) = expected_rcfile_type(key) else {
            continue;
        };
        let accepted = match expected {
            RcfileType::Boolean => val.is_boolean(),
            RcfileType::String => val.is_string(),
            RcfileType::Port => val.as_u64().is_some_and(|p| u16::try_from(p).is_ok()),
        };
        if !accepted {
            // The offending value is echoed so the operator can see what was wrong with it —
            // except for a credential key, where the wrong-typed value is most likely the token
            // itself (an unquoted one is exactly the mistake this catches), and this message
            // reaches stderr, any `2>` redirect and CI output. Name the type it had instead. Same
            // reasoning as `ServeOptions` in rift-ffi, which does not derive `Debug` because it
            // holds `api_key`.
            let got = if rcfile_key_is_secret(key) {
                describe_json_type(val).to_string()
            } else {
                val.to_string()
            };
            anyhow::bail!(
                "rcfile {}: '{key}' must be {}, got {got}. Refusing the rcfile rather than \
                 ignoring or misreading the value.",
                rcfile.display(),
                expected.describe()
            );
        }
    }

    let mut unsupported = Vec::new();
    for (key, val) in map {
        match key.as_str() {
            "port" => {
                if cli.port == DEFAULT_ADMIN_PORT
                    && let Some(p) = val.as_u64().and_then(|p| u16::try_from(p).ok())
                {
                    cli.port = p;
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
            // Types were checked above, so these reads cannot miss.
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
            "requireAdminAuth" | "require_admin_auth" => {
                if !cli.require_admin_auth {
                    cli.require_admin_auth = val.as_bool().unwrap_or(false);
                }
            }
            // The credential the key above gates on (issue #1132). Ignoring an unrecognised key is
            // right; ignoring this one produced an advisory that read as reassurance and a server
            // with no key — and paired with `requireAdminAuth` it refused to start, telling the
            // operator to set `--api-key`, from a file that had set it. A blank value is not
            // rejected here: it is a valid string, and `validate_admin_api_key` is the one place
            // that judges it, for the flag and the file alike.
            "apiKey" | "api_key" => {
                if cli.api_key.is_none()
                    && let Some(k) = val.as_str()
                {
                    cli.api_key = Some(k.to_string());
                }
            }
            "datadir" => {
                if cli.datadir.is_none()
                    && let Some(d) = val.as_str()
                {
                    cli.datadir = Some(std::path::PathBuf::from(d));
                }
            }
            "noParse" | "no_parse" => {
                if !cli.no_parse {
                    cli.no_parse = val.as_bool().unwrap_or(false);
                }
            }
            "configfile" => {
                if cli.configfile.is_none()
                    && let Some(f) = val.as_str()
                {
                    cli.configfile = Some(std::path::PathBuf::from(f));
                }
            }
            other => unsupported.push(other.to_string()),
        }
    }
    Ok(unsupported)
}

/// The JSON type an rcfile key must have. `None` for a key the rcfile does not recognise.
#[derive(Debug, Clone, Copy)]
enum RcfileType {
    Boolean,
    String,
    Port,
}

impl RcfileType {
    fn describe(self) -> &'static str {
        match self {
            RcfileType::Boolean => "a JSON boolean",
            RcfileType::String => "a JSON string",
            RcfileType::Port => "an integer from 0 to 65535",
        }
    }
}

/// Whether an rcfile key's *value* is a secret that must never be echoed in an error or a log.
fn rcfile_key_is_secret(key: &str) -> bool {
    matches!(key, "apiKey" | "api_key")
}

/// The JSON type a value actually has, for an error that must not quote the value itself.
fn describe_json_type(val: &serde_json::Value) -> &'static str {
    match val {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn expected_rcfile_type(key: &str) -> Option<RcfileType> {
    match key {
        "allowInjection" | "allow_injection" | "localOnly" | "local_only" | "requireAdminAuth"
        | "require_admin_auth" | "noParse" | "no_parse" => Some(RcfileType::Boolean),
        "host" | "logLevel" | "loglevel" | "datadir" | "configfile" | "apiKey" | "api_key" => {
            Some(RcfileType::String)
        }
        "port" => Some(RcfileType::Port),
        _ => None,
    }
}

/// Default PID file for the `stop`/`restart` subcommands when `--pidfile` is not given.
///
/// Deliberately applied at the dispatch site rather than as a clap `default_value` on the global
/// `--pidfile`: a default on the flag itself would make every plain `rift` start write a PID file
/// it never wrote before (issue #827).
pub const DEFAULT_PIDFILE: &str = "rift.pid";

/// Stop a running server for `restart`: a missing PID file is a satisfied precondition.
///
/// `restart` means "end up running". If there is no PID file there is nothing to stop and the
/// desired end state already holds, so this reports `Ok` and lets the caller start — whereas bare
/// [`stop_server`] keeps its hard error, since a `stop` with nothing to stop is a user error
/// (issue #827).
pub fn stop_for_restart(pidfile: &Path) -> Result<(), anyhow::Error> {
    if !pidfile.exists() {
        info!("no PID file at {pidfile:?}; nothing to stop, starting fresh");
        return Ok(());
    }
    stop_server(pidfile)
}

/// Stop a running server by PID file.
///
/// Idempotent about the end state, loud about everything else: a stale pidfile (the process is
/// already gone) is cleaned up and reported `Ok`, but a signal that is denied (the process is not
/// ours) or fails unexpectedly is an error and the pidfile is left in place — it is not stale.
/// A pidfile whose PID is non-positive is rejected outright (it names a process *group*, not a
/// process) and likewise kept.
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

    // A pidfile must name a specific process. `kill(0, ..)` signals every process in the caller's
    // own group and `kill(-pgid, ..)` broadcasts to a group, so a corrupt or crafted pidfile with a
    // non-positive pid could make `rift stop` SIGTERM itself. Refuse it — and keep the pidfile,
    // since we never acted on it.
    if pid <= 0 {
        return Err(anyhow::anyhow!(
            "refusing to signal non-positive pid {pid} from PID file {pidfile:?}"
        ));
    }

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

#[cfg(test)]
mod tests {
    use super::apply_rcfile_defaults;
    use crate::server::Cli;
    use clap::Parser;

    // Issue #1114: the binary now reports unsupported keys itself; an embedder calling the plain
    // function with its own subscriber must still see them logged.
    #[test]
    #[tracing_test::traced_test]
    fn apply_rcfile_defaults_still_logs_unsupported_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rcfile = dir.path().join("rift.rc");
        std::fs::write(&rcfile, r#"{"bogusKey": 1}"#).expect("write rcfile");
        let mut cli = Cli::try_parse_from(["rift"]).expect("cli parse");
        apply_rcfile_defaults(&mut cli, &rcfile).expect("unknown keys are not fatal");
        assert!(logs_contain("unsupported key 'bogusKey'"));
    }
}
