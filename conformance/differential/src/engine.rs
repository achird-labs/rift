//! Spawning the two engines and talking HTTP to them.

use crate::case::Scheme;
use crate::diff::{FailureClass, Observation, normalize_headers};
use crate::ports::PortMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Mountebank,
    Rift,
}

/// A running engine. [`Engine::stop`] shuts it down; the process is also killed when this is
/// dropped (the backstop for a panic, which loses Mountebank's coverage).
#[derive(Debug)]
pub struct Engine {
    pub which: Which,
    pub admin_port: u16,
    child: Child,
    pidfile: Option<PathBuf>,
    /// Mountebank's `mb`, for `mb stop`.
    mb: Option<PathBuf>,
    /// Mountebank runs under `NODE_V8_COVERAGE`, so a stop that does not let it exit cleanly loses
    /// the coverage and is an error.
    coverage: bool,
}

/// How long Mountebank gets to close its server and exit after `mb stop`.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

impl Engine {
    /// Shuts the engine down. Rift is killed. Mountebank is stopped with `mb stop`, the SIGTERM
    /// its shutdown handler answers with `process.exit()`, which is when node writes the
    /// `NODE_V8_COVERAGE` profile; a kill would lose it.
    ///
    /// # Errors
    /// When the kill fails, or, for a Mountebank run under coverage, when `mb stop` fails or
    /// Mountebank has not exited cleanly within the stop window.
    pub async fn stop(&mut self) -> Result<(), String> {
        let (Some(mb), Some(pidfile)) = (&self.mb, &self.pidfile) else {
            return self
                .child
                .kill()
                .await
                .map_err(|e| format!("kill {:?}: {e}", self.which));
        };
        let stopped = Command::new(mb)
            .arg("stop")
            .arg("--pidfile")
            .arg(pidfile)
            // The stop command is a second node process; its profile is not Mountebank serving.
            .env_remove("NODE_V8_COVERAGE")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .await;
        let exited = match stopped {
            Ok(status) if status.success() => {
                tokio::time::timeout(STOP_TIMEOUT, self.child.wait()).await
            }
            Ok(status) => {
                return self
                    .stop_failed(format!("`{} stop` exited with {status}", mb.display()))
                    .await;
            }
            Err(e) => {
                return self
                    .stop_failed(format!("spawn `{} stop`: {e}", mb.display()))
                    .await;
            }
        };
        match exited {
            Ok(Ok(status)) if status.success() || !self.coverage => Ok(()),
            Ok(Ok(status)) => Err(format!(
                "Mountebank exited with {status} after `mb stop`: its coverage profile may be \
                 incomplete"
            )),
            Ok(Err(e)) => self.stop_failed(format!("wait for Mountebank: {e}")).await,
            Err(_) => {
                self.stop_failed(format!(
                    "Mountebank did not exit within {}s of `mb stop`",
                    STOP_TIMEOUT.as_secs()
                ))
                .await
            }
        }
    }

    /// Kills a Mountebank that would not stop. Without coverage that is the whole job; with
    /// coverage the profile is lost, so it is still an error.
    async fn stop_failed(&mut self, why: String) -> Result<(), String> {
        let killed = self.child.kill().await;
        match (self.coverage, killed) {
            (true, _) => Err(format!("{why}; killed it, losing its coverage profile")),
            (false, Ok(())) => Ok(()),
            (false, Err(e)) => Err(format!("{why}; kill: {e}")),
        }
    }
}

/// `RIFT_DIFFERENTIAL_MB_COVERAGE`: the directory Mountebank writes its V8 coverage profile to,
/// created if missing. `None` when unset.
///
/// # Errors
/// When the path is relative (node resolves it against Mountebank's working directory, not
/// the caller's) or cannot be created.
pub fn mb_coverage_dir() -> Result<Option<PathBuf>, String> {
    let Some(dir) = std::env::var_os("RIFT_DIFFERENTIAL_MB_COVERAGE") else {
        return Ok(None);
    };
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        return Err(format!(
            "RIFT_DIFFERENTIAL_MB_COVERAGE={} must be an absolute path",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("RIFT_DIFFERENTIAL_MB_COVERAGE={}: {e}", dir.display()))?;
    Ok(Some(dir))
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(pidfile) = &self.pidfile {
            // Best-effort cleanup of a temp file; a leftover pidfile harms nothing.
            let _ = std::fs::remove_file(pidfile);
        }
    }
}

/// Where Mountebank is: `RIFT_MB_BIN`, else `~/bench-mb/node_modules/.bin/mb` (the bench layout,
/// `docs/performance/index.md`), else `mb` on `PATH`. `None` when none exists.
///
/// # Panics
/// When `RIFT_MB_BIN` is set but names no file — quietly falling back would test something else.
#[must_use]
pub fn locate_mountebank() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RIFT_MB_BIN") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "RIFT_MB_BIN={} does not name a file",
            path.display()
        );
        return Some(path);
    }
    let bench = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("bench-mb/node_modules/.bin/mb"));
    if let Some(bench) = bench.filter(|p| p.is_file()) {
        return Some(bench);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("mb"))
            .find(|candidate| candidate.is_file())
    })
}

/// The Rift server binary: `RIFT_SERVER_BIN` (the same override the `rift-http-proxy` suites
/// honour), else `rift-http-proxy` in the target directory this test binary was built into.
///
/// # Errors
/// When neither exists; the message says how to build it.
pub fn locate_rift() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("RIFT_SERVER_BIN") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "RIFT_SERVER_BIN={} does not name a file",
                path.display()
            ))
        };
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // target/<profile>/deps/<test-binary> → target/<profile>/rift-http-proxy
    let candidate = exe
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join(format!("rift-http-proxy{}", std::env::consts::EXE_SUFFIX)))
        .ok_or_else(|| format!("unexpected test binary location {}", exe.display()))?;
    if candidate.is_file() {
        refuse_stale(&candidate)?;
        Ok(candidate)
    } else {
        Err(format!(
            "no Rift server at {}: build it first (`cargo build -p rift-http-proxy`, which \
             `cargo test --workspace` does), or point RIFT_SERVER_BIN at one",
            candidate.display()
        ))
    }
}

/// `cargo test -p rift-differential` does not rebuild the server (it is not a dependency), so a
/// binary older than the engine sources would test yesterday's Rift and report on it as today's.
fn refuse_stale(binary: &Path) -> Result<(), String> {
    let built = binary
        .metadata()
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    match newest_source(&crates) {
        Some((path, modified)) if modified > built => Err(format!(
            "{} is older than {}: rebuild it (`cargo build -p rift-http-proxy`) or set \
             RIFT_SERVER_BIN",
            binary.display(),
            path.display()
        )),
        _ => Ok(()),
    }
}

fn newest_source(dir: &Path) -> Option<(PathBuf, std::time::SystemTime)> {
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let candidate = if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "tests" || n == "benches")
            {
                continue;
            }
            newest_source(&path)
        } else if path.extension().is_some_and(|e| e == "rs") {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|t| (path, t))
        } else {
            None
        };
        if let Some(found) = candidate
            && newest.as_ref().is_none_or(|(_, t)| found.1 > *t)
        {
            newest = Some(found);
        }
    }
    newest
}

/// Reserves `n` distinct free ports. The listeners are held until all are chosen, so the ports are
/// distinct; they are released before the engines bind them.
///
/// Fixed ports, probed (#1485): a case's documents name their imposters' ports, and Mountebank's
/// admin port is set on its command line (it cannot report a port it chose). They are probed on the
/// wildcard address both engines bind imposters (and Mountebank its admin API) on.
///
/// # Errors
/// When the OS cannot hand out a port.
pub fn free_ports(n: usize) -> std::io::Result<Vec<u16>> {
    let listeners = (0..n)
        .map(|_| std::net::TcpListener::bind("0.0.0.0:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    listeners
        .iter()
        .map(|l| l.local_addr().map(|a| a.port()))
        .collect()
}

/// Starts Mountebank with injection on and no log/pid files in the working directory. With
/// `RIFT_DIFFERENTIAL_MB_COVERAGE` set, node writes a V8 coverage profile there when Mountebank
/// is stopped with [`Engine::stop`]: `mb` is a `#!/usr/bin/env node` script, so the child is node
/// itself and needs no wrapper.
///
/// # Errors
/// A bad coverage directory, spawn failure, or the admin API not answering within the startup
/// window.
pub async fn start_mountebank(mb: &Path, cwd: &Path) -> Result<Engine, String> {
    let coverage = mb_coverage_dir()?;
    let admin_port = free_ports(1).map_err(|e| e.to_string())?[0];
    let pidfile = std::env::temp_dir().join(format!(
        "rift-differential-mb-{}-{admin_port}.pid",
        std::process::id()
    ));
    let mut command = Command::new(mb);
    if let Some(dir) = &coverage {
        command.env("NODE_V8_COVERAGE", dir);
    }
    let child = command
        .args(["start", "--port", &admin_port.to_string()])
        .args([
            "--allowInjection",
            "--localOnly",
            "--loglevel",
            "warn",
            "--nologfile",
        ])
        .arg("--pidfile")
        .arg(&pidfile)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", mb.display()))?;
    let mut engine = Engine {
        which: Which::Mountebank,
        admin_port,
        child,
        pidfile: Some(pidfile),
        mb: Some(mb.to_path_buf()),
        coverage: coverage.is_some(),
    };
    wait_ready(&mut engine).await?;
    Ok(engine)
}

/// The admin API's startup line, logged at `info` by this target, carries the port it bound.
const ADMIN_LISTENING_TARGET: &str = "rift_http_proxy::admin_api::server";
const ADMIN_LISTENING: &str = "listening on http://";

/// Starts Rift with the flags that mirror Mountebank's: injection on, localhost only. Plus
/// `--scripts-dir <scripts_dir>`, which Mountebank has no counterpart for: without it an admin-API
/// `lookup` reads no CSV file at all (rift#1617).
///
/// The admin API binds port 0 and the port is read from its startup line, so no probe is ever
/// released for something else to take (#1485). Everything else logs at `error`, as before.
///
/// # Errors
/// Spawn failure, the admin port not reported, or the admin API not answering within the startup
/// window.
pub async fn start_rift(bin: &Path, cwd: &Path, scripts_dir: &Path) -> Result<Engine, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = Command::new(bin)
        .args(["--port", "0", "--allow-injection", "--local-only"])
        .arg("--scripts-dir")
        .arg(scripts_dir)
        .args(["--loglevel", "error"])
        .env("RUST_LOG", format!("error,{ADMIN_LISTENING_TARGET}=info"))
        .env("NO_COLOR", "1")
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let stdout = child.stdout.take().ok_or("rift stdout was not piped")?;
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut port_tx = Some(port_tx);
        // Read to the end either way, so the child never blocks on a full pipe.
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(port) = admin_port_from(&line)
                && let Some(tx) = port_tx.take()
            {
                // The receiver is gone only once start_rift gave up waiting.
                let _ = tx.send(port);
            }
        }
    });
    let admin_port = match tokio::time::timeout(Duration::from_secs(30), port_rx).await {
        Ok(Ok(port)) => port,
        Ok(Err(_)) => return Err("Rift exited before reporting its admin port".to_string()),
        Err(_) => return Err("Rift did not report its admin port within 30s".to_string()),
    };
    let mut engine = Engine {
        which: Which::Rift,
        admin_port,
        child,
        pidfile: None,
        mb: None,
        coverage: false,
    };
    wait_ready(&mut engine).await?;
    Ok(engine)
}

/// The port in `... listening on http://<host>:<port>`, colour codes ignored.
fn admin_port_from(line: &str) -> Option<u16> {
    let plain: String = {
        let mut out = String::with_capacity(line.len());
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let rest = plain.split(ADMIN_LISTENING).nth(1)?;
    let addr = rest.split_whitespace().next()?;
    addr.rsplit(':').next()?.parse().ok()
}

async fn wait_ready(engine: &mut Engine) -> Result<(), String> {
    let client = client().map_err(|e| e.to_string())?;
    let url = format!("http://127.0.0.1:{}/", engine.admin_port);
    for _ in 0..300 {
        if let Ok(Some(status)) = engine.child.try_wait() {
            return Err(format!(
                "{:?} exited during startup: {status}",
                engine.which
            ));
        }
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "{:?} admin API on port {} did not come up within 30s",
        engine.which, engine.admin_port
    ))
}

/// The HTTP client both engines are driven with: no redirects, no proxy, no pooling (a fault that
/// kills a connection must not poison the next request), a generous per-request timeout. It
/// accepts any certificate: an `https` imposter serves its engine's self-signed default (or the
/// case's own), and the harness compares what is served over TLS, not who signed it.
///
/// # Errors
/// When reqwest cannot build a client.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(30))
        .tls_danger_accept_invalid_certs(true)
        .build()
}

/// A request already mapped to one engine's ports.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub scheme: Scheme,
    pub port: u16,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// Sends one request and observes the answer, mapping ports in headers and body back to logical.
///
/// # Panics
/// When the request's method is not a valid HTTP method (a malformed case).
pub async fn send(client: &reqwest::Client, request: &Outgoing, ports: &PortMap) -> Observation {
    // A malformed method is a broken case file, not an engine answer: fail loudly.
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .unwrap_or_else(|_| panic!("case uses an invalid HTTP method {:?}", request.method));
    let url = format!(
        "{}://127.0.0.1:{}{}",
        request.scheme.as_str(),
        request.port,
        request.path
    );
    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(e) => return Observation::Failed(classify(&e)),
    };
    let status = response.status().as_u16();
    let headers = normalize_headers(
        response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), String::from_utf8_lossy(value.as_bytes()))),
        ports,
    );
    match response.bytes().await {
        Ok(bytes) => Observation::Response {
            status,
            headers,
            body: reverse_body(&bytes, ports),
        },
        Err(e) => Observation::Failed(classify(&e)),
    }
}

/// Maps ports in a body back to logical: a JSON body as a value (keeping its exact text when no
/// port occurs, so key order and number formatting survive for the key-order check), else as text.
fn reverse_body(bytes: &[u8], ports: &PortMap) -> Vec<u8> {
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(bytes) {
        let mapped = ports.reverse_value(&json);
        if mapped == json {
            return bytes.to_vec();
        }
        return serde_json::to_vec(&mapped).expect("a serde_json::Value always serialises");
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => ports.reverse_str(text).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

fn classify(error: &reqwest::Error) -> FailureClass {
    if error.is_timeout() {
        FailureClass::Timeout
    } else if error.is_connect() {
        FailureClass::Connect
    } else {
        FailureClass::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::admin_port_from;

    #[test]
    fn admin_port_from_reads_the_admin_startup_line() {
        let line = "\u{1b}[2m2026-10-16T00:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
                    rift_http_proxy::admin_api::server: Rift Admin API (Mountebank-compatible) \
                    listening on http://127.0.0.1:51234";
        assert_eq!(admin_port_from(line), Some(51234));
        assert_eq!(admin_port_from("starting"), None);
    }
}
