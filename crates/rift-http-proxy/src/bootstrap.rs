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
use tracing_subscriber::EnvFilter;

/// The environment variable `EnvFilter` reads by default, named here because this module has to
/// tell "unset" apart from "set but unparseable" itself.
const RUST_LOG: &str = "RUST_LOG";

/// The `--loglevel` values this binary accepts, in the order an operator would think of them.
///
/// Stated in two other places that nothing gates against this one — the `--loglevel` doc comment in
/// `server.rs` (which `--help` renders) and the options table in `docs/configuration/cli.md`.
/// `scripts/verify-docs-coverage.sh` checks that a flag is *documented*, not that its help text
/// matches, so all three drift silently. Change them together.
const ACCEPTED_LEVELS: &str = "trace, debug, info, warn (or warning), error";

/// The tracing filter this CLI asks for: `RUST_LOG` when it is set, otherwise `--debug`, otherwise
/// `--loglevel`.
///
/// Public so an alternative binary applies the same rules rather than copying them (issue #1134) —
/// the copy in rift-cluster's `rift-cluster-server` is what this exists to replace.
///
/// # Errors
/// - `--loglevel` names a level that does not exist. It used to fall through to `info`, so `trace`
///   (a real level) and a typo were equally silent.
/// - `RUST_LOG` is set and does not parse, or is not valid UTF-8. Both used to be indistinguishable
///   from *unset* and were replaced by the CLI level with nothing said. Unset remains a
///   domain-optional absence and is not an error.
pub fn log_filter(cli: &Cli) -> anyhow::Result<EnvFilter> {
    let rust_log = match std::env::var(RUST_LOG) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        // Set, but unreadable. Treated as the same class as unparseable rather than as absence:
        // the operator did configure something, and quietly ignoring it is the bug being fixed.
        Err(std::env::VarError::NotUnicode(raw)) => anyhow::bail!(
            "{RUST_LOG} is set to a value that is not valid UTF-8 ({raw:?}), so it cannot be read \
             as a tracing filter. Unset it to fall back to --loglevel."
        ),
    };
    log_filter_with(cli, rust_log.as_deref())
}

/// [`log_filter`] with the `RUST_LOG` value supplied rather than read: `None` means unset.
///
/// Split out so the rules are testable without mutating the process environment, which is global
/// and shared by every test in a binary.
pub fn log_filter_with(cli: &Cli, rust_log: Option<&str>) -> anyhow::Result<EnvFilter> {
    // Validated even when `RUST_LOG` is about to supersede it: a level that does not exist is a
    // mistake worth reporting either way, and refusing only when the value happens to be used would
    // make the same command line succeed or fail depending on the environment.
    let level = match cli.loglevel.trim().to_lowercase().as_str() {
        // An empty value means "not supplied", not "a level I could not read". `MB_LOGLEVEL` is a
        // clap `env`, and clap prefers a *present* variable over the default even when it is empty
        // — so `MB_LOGLEVEL=${LOG_LEVEL}` in a compose file with `LOG_LEVEL` unset arrives here as
        // "". Refusing it would abort deployments that work today, to report a typo nobody made.
        "" => "info",
        "trace" => "trace",
        "debug" => "debug",
        "info" => "info",
        "warn" | "warning" => "warn",
        "error" => "error",
        // Echo what the operator actually typed, not the lowercased form we matched on — they will
        // be looking for their own string in the message.
        _ => anyhow::bail!(
            "--loglevel {:?} is not a log level. Accepted: {ACCEPTED_LEVELS}.",
            cli.loglevel
        ),
    };

    match rust_log {
        Some(value) => EnvFilter::try_new(value).with_context(|| {
            format!("{RUST_LOG} is set to {value:?}, which is not a valid tracing filter")
        }),
        // `try_new` rather than `new`, which panics on a bad directive. This one cannot fail — every
        // arm above is a validated literal — but a panicking level construction is the exact defect
        // this seam exists to remove, and `clippy::panic` does not see into a callee.
        None => {
            let level = if cli.debug { "debug" } else { level };
            EnvFilter::try_new(level)
                .with_context(|| format!("building a tracing filter for --loglevel {level:?}"))
        }
    }
}

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
///
/// Waits up to five seconds for the process to exit, above this server's own shutdown bound. An
/// embedder whose server takes longer to leave on SIGTERM calls [`stop_server_within`] instead.
pub fn stop_server(pidfile: &Path) -> Result<(), anyhow::Error> {
    stop_server_within(pidfile, STOP_WAIT)
}

/// [`stop_server`] with the exit wait chosen by the caller.
///
/// `ceiling` bounds how long the stop waits, after SIGTERM, for the process to be gone; a process
/// still alive past it is an error and its PID file is kept. It exists for an embedder whose server
/// does more than this crate's on SIGTERM — a drain window or a cluster departure — and so
/// legitimately outlives [`stop_server`]'s fixed five seconds: with that ceiling its `stop` reported
/// failure, and its `restart` never started, for every graceful shutdown. Unused on Windows, where
/// `taskkill /F` does not wait on the process's own shutdown.
pub fn stop_server_within(
    pidfile: &Path,
    #[cfg_attr(not(unix), allow(unused_variables))] ceiling: std::time::Duration,
) -> Result<(), anyhow::Error> {
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
        if rc == 0 {
            // Wait for the process to actually go (issue #1155). `stop` used to return the moment
            // the signal was sent — reporting success while the server was still running, which
            // made `restart` race its own rebind into EADDRINUSE once the server started shutting
            // down gracefully. The ceiling sits above the server's roughly three-second bound; a
            // process still alive past it (an old, handler-less PID-1 server, say) is reported
            // rather than assumed gone, and its PID file is kept.
            if !wait_for_exit(pid, ceiling, STOP_POLL) {
                return Err(anyhow::anyhow!(
                    "process {pid} did not exit within {ceiling:?} of SIGTERM; leaving PID file in \
                     place"
                ));
            }
        }
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

    // Remove PID file (success path, and the ESRCH stale-pidfile path). Already gone is success:
    // since issue #1155 a server removes its own PID file on the way out, so it usually gets there
    // before we do, and a bare `remove_file(..)?` would turn a clean stop into a spurious failure.
    match std::fs::remove_file(pidfile) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

/// How long `rift stop` waits for the process to exit — above the server's shutdown bound.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Poll until `pid` has exited or `ceiling` passes. `true` when it exited.
#[cfg(unix)]
fn wait_for_exit(pid: i32, ceiling: std::time::Duration, poll: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        if has_exited(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// Whether `pid` has exited — including when it is the **caller's own child** that has not been
/// reaped yet.
///
/// Signal 0 is the ordinary probe: `ESRCH` means gone; `EPERM` means it exists but belongs to someone
/// else, so it is still running. But a child that has exited and not been reaped is a zombie, and a
/// zombie still answers signal 0 — so a caller that is the target's parent (an embedder that spawned
/// the server and calls [`stop_server`] in-process) would wait out the whole ceiling for a process
/// that is already dead. `waitid` with `WNOWAIT` reports that exit **without reaping it**, so the
/// owner's own `wait` still gets the exit status. For a process that is not our child it answers
/// `ECHILD`, and the signal-0 probe decides.
#[cfg(unix)]
fn has_exited(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the process exists; it touches no memory.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    let Ok(id) = libc::id_t::try_from(pid) else {
        return false;
    };
    // SAFETY: a zeroed `siginfo_t` is a valid out-parameter; waitid(2) writes into it and reads
    // nothing else. WNOHANG keeps it non-blocking and WNOWAIT leaves the child waitable.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: `info` is a zero-initialised `siginfo_t`, so reading `si_pid` is defined whatever
    // waitid did. With WNOHANG and no exited child, POSIX leaves `si_pid` unspecified in principle;
    // Linux and macOS leave the buffer untouched, so the zero-init is what makes it read as pid 0 —
    // never `pid` — and correctness rests on that initialisation.
    rc == 0 && unsafe { info.si_pid() } == pid
}

/// The replayable-imposters endpoint of the admin API at `host:port`.
fn save_url(host: &str, port: u16, remove_proxies: bool) -> String {
    let authority = crate::healthcheck::url_authority(host, port);
    let query = if remove_proxies {
        "replayable=true&removeProxies=true"
    } else {
        "replayable=true"
    };
    format!("http://{authority}/imposters?{query}")
}

/// Save imposters to a file (async form).
///
/// Fetches the replayable imposter config from the admin API at `host:port` and writes it to
/// `savefile`. This is the form to call from an embedder's own async runtime — it awaits rather
/// than driving a nested runtime, so it is safe on an async worker thread. Sync callers (the `save`
/// subcommand) should use [`save_imposters`], which wraps this.
///
/// `api_key` is presented as the raw `Authorization` value when the server is keyed (issue #1154) —
/// without it `rift save` against a server started with `--api-key` / `MB_APIKEY` was a 401.
pub async fn save_imposters_async(
    host: &str,
    port: u16,
    savefile: &Path,
    remove_proxies: bool,
    api_key: Option<&str>,
) -> Result<(), anyhow::Error> {
    let client = reqwest::Client::new();
    let url = save_url(host, port, remove_proxies);

    let mut request = client.get(&url);
    if let Some(key) = api_key {
        request = request.header(
            reqwest::header::AUTHORIZATION,
            crate::healthcheck::sensitive_header(key)?,
        );
    }
    // `error_for_status` before `.text()` so a 401/500 response is a value error, not a body
    // silently written to the user's savefile. The error carries the status and URL.
    let response = request.send().await?.error_for_status()?;
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
    api_key: Option<&str>,
) -> Result<(), anyhow::Error> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(save_imposters_async(
        host,
        port,
        savefile,
        remove_proxies,
        api_key,
    ))
}

#[cfg(test)]
mod tests {
    use super::{apply_rcfile_defaults, log_filter_with, save_url};
    use crate::server::Cli;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["rift"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("cli parse")
    }

    /// The filter's rendered form. `EnvFilter` has no accessor for its directives, but its `Display`
    /// is the directive list — which is what an operator set and what we are asserting about.
    fn rendered(cli: &Cli, rust_log: Option<&str>) -> String {
        log_filter_with(cli, rust_log)
            .expect("filter must build")
            .to_string()
    }

    // AC1: `trace` is a real tracing level and a plausible thing to ask for. It used to land in the
    // catch-all arm and silently become `info`.
    #[test]
    fn trace_is_accepted() {
        assert_eq!(rendered(&cli(&["--loglevel", "trace"]), None), "trace");
    }

    #[test]
    fn every_documented_level_round_trips() {
        for (given, expected) in [
            ("trace", "trace"),
            ("debug", "debug"),
            ("info", "info"),
            ("warn", "warn"),
            ("warning", "warn"),
            ("error", "error"),
        ] {
            assert_eq!(
                rendered(&cli(&["--loglevel", given]), None),
                expected,
                "--loglevel {given}"
            );
        }
    }

    #[test]
    fn the_level_is_case_insensitive() {
        for given in ["TRACE", "Trace", "tRaCe"] {
            assert_eq!(rendered(&cli(&["--loglevel", given]), None), "trace");
        }
    }

    #[test]
    fn the_default_cli_is_info() {
        assert_eq!(rendered(&cli(&[]), None), "info");
    }

    // AC2: the #1114 judgement, applied to a wrong *value* rather than a wrong type — a mistyped
    // level used to start the server at `info` with nothing said.
    #[test]
    fn an_unrecognised_level_is_refused_naming_the_value() {
        for bad in ["warnn", "verbose", "1"] {
            let err = log_filter_with(&cli(&["--loglevel", bad]), None)
                .expect_err("an unrecognised --loglevel must be refused, not defaulted to info");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("--loglevel"),
                "the refusal must name the flag, got: {msg}"
            );
            assert!(
                msg.contains("trace") && msg.contains("error"),
                "the refusal must list the accepted levels, got: {msg}"
            );
            assert!(
                msg.contains(bad),
                "the refusal must name the offending value, got: {msg}"
            );
        }
    }

    // An empty or whitespace-only value means "not supplied", not "a level I could not read".
    // `MB_LOGLEVEL` is a clap `env`, and clap prefers a *present* variable over the default even
    // when it is empty — so `MB_LOGLEVEL=${LOG_LEVEL}` with `LOG_LEVEL` unset arrives as "".
    // Refusing that would abort deployments that work today in order to report a typo nobody made.
    #[test]
    fn an_empty_level_means_not_supplied() {
        for empty in ["", "   "] {
            assert_eq!(
                rendered(&cli(&["--loglevel", empty]), None),
                "info",
                "--loglevel {empty:?} must mean 'not supplied'"
            );
        }
    }

    // The refusal quotes what the operator typed, not the lowercased form matched on — they will be
    // scanning the message for their own string.
    #[test]
    fn the_refusal_echoes_the_original_casing() {
        let err = log_filter_with(&cli(&["--loglevel", "WaRnN"]), None)
            .expect_err("still refused regardless of casing");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("WaRnN"),
            "the refusal must quote the value as given, got: {msg}"
        );
    }

    // AC6: `--debug` outranks `--loglevel` — preserved from the code this seam replaces.
    #[test]
    fn debug_outranks_the_level() {
        assert_eq!(
            rendered(&cli(&["--debug", "--loglevel", "error"]), None),
            "debug"
        );
    }

    // AC6: and RUST_LOG outranks both — also preserved.
    #[test]
    fn rust_log_outranks_the_cli() {
        assert_eq!(
            rendered(&cli(&["--loglevel", "error"]), Some("warn")),
            "warn"
        );
        assert_eq!(rendered(&cli(&["--debug"]), Some("warn")), "warn");
    }

    #[test]
    fn rust_log_carries_a_full_directive_set() {
        // `EnvFilter`'s `Display` sorts directives rather than preserving input order, so assert on
        // the set rather than on the rendering.
        let rendered = rendered(&cli(&[]), Some("info,hyper=off"));
        assert!(rendered.contains("info"), "{rendered}");
        assert!(rendered.contains("hyper=off"), "{rendered}");
    }

    // AC3: the half of the bug that had no spelling at all — a set-but-unparseable RUST_LOG was
    // indistinguishable from an unset one, so the operator's filter was silently discarded.
    //
    // `foo=bar` is invalid because `bar` is not a level. Note the issue's own example, `RUST_LOG=inf`,
    // is NOT invalid — see the test below.
    #[test]
    fn an_unparseable_rust_log_is_refused() {
        let err = log_filter_with(&cli(&[]), Some("foo=bar"))
            .expect_err("a set-but-unparseable RUST_LOG must be refused, not silently replaced");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RUST_LOG"),
            "the refusal must name the variable, got: {msg}"
        );
        assert!(
            msg.contains("foo=bar"),
            "the refusal must name the offending value, got: {msg}"
        );
    }

    // Recorded because issue #1134 cites `RUST_LOG=inf` as its example of an unparseable filter, and
    // it is not one: in `EnvFilter` syntax a bare word is a *target* directive, so `inf` is a valid
    // filter meaning "the target named `inf`". It always parsed, and it is not what the refusal
    // above is for. Pinned so nobody later "fixes" this into a refusal and breaks real filters like
    // `RUST_LOG=my_crate`.
    #[test]
    fn a_bare_word_rust_log_is_a_target_directive_not_an_error() {
        log_filter_with(&cli(&[]), Some("inf"))
            .expect("a bare word is a target name, which is a valid filter");
    }

    // AC4: absence is a domain value, not a failure — the one silent default that stays.
    #[test]
    fn an_unset_rust_log_is_not_an_error() {
        assert_eq!(rendered(&cli(&["--loglevel", "warn"]), None), "warn");
    }

    // Deliberately unchanged: `RUST_LOG=` (set, empty) parses to an empty directive set today, and
    // this seam keeps that meaning rather than quietly reclassifying it as "unset".
    #[test]
    fn an_empty_rust_log_keeps_its_current_meaning() {
        log_filter_with(&cli(&[]), Some(""))
            .expect("an empty RUST_LOG is set-to-nothing, not unset, and is not an error");
    }

    // A typo is a typo whether or not RUST_LOG would have superseded it. Refusing only when the
    // value happens to be used would make the same input succeed or fail based on the environment.
    #[test]
    fn a_bad_level_is_refused_even_when_rust_log_would_supersede_it() {
        log_filter_with(&cli(&["--loglevel", "warnn"]), Some("debug"))
            .expect_err("an unrecognised --loglevel must be refused regardless of RUST_LOG");
    }

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

    // Issue #1137: `rift save --host ::1` built `http://::1:2525/...`, which no client can parse.
    #[test]
    fn save_url_brackets_a_bare_ipv6_host() {
        assert_eq!(
            save_url("::1", 2525, false),
            "http://[::1]:2525/imposters?replayable=true"
        );
        assert_eq!(
            save_url("[::1]", 2525, true),
            "http://[::1]:2525/imposters?replayable=true&removeProxies=true"
        );
        assert_eq!(
            save_url("localhost", 2525, false),
            "http://localhost:2525/imposters?replayable=true"
        );
    }
}
