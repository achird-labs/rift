//! Spawning the two engines and talking HTTP to them.

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

/// A running engine. The process is killed when this is dropped.
#[derive(Debug)]
pub struct Engine {
    pub which: Which,
    pub admin_port: u16,
    child: Child,
    pidfile: Option<PathBuf>,
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
            "{} is older than {}: rebuild it (`cargo build -p rift-http-proxy`) or set \\
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
/// # Errors
/// When the OS cannot hand out a port.
pub fn free_ports(n: usize) -> std::io::Result<Vec<u16>> {
    let listeners = (0..n)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    listeners
        .iter()
        .map(|l| l.local_addr().map(|a| a.port()))
        .collect()
}

/// Starts Mountebank with injection on and no log/pid files in the working directory.
///
/// # Errors
/// Spawn failure, or the admin API not answering within the startup window.
pub async fn start_mountebank(mb: &Path, cwd: &Path) -> Result<Engine, String> {
    let admin_port = free_ports(1).map_err(|e| e.to_string())?[0];
    let pidfile = std::env::temp_dir().join(format!(
        "rift-differential-mb-{}-{admin_port}.pid",
        std::process::id()
    ));
    let child = Command::new(mb)
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
    };
    wait_ready(&mut engine).await?;
    Ok(engine)
}

/// Starts Rift with the flags that mirror Mountebank's: injection on, localhost only.
///
/// # Errors
/// Spawn failure, or the admin API not answering within the startup window.
pub async fn start_rift(bin: &Path, cwd: &Path) -> Result<Engine, String> {
    let admin_port = free_ports(1).map_err(|e| e.to_string())?[0];
    let child = Command::new(bin)
        .args([
            "--port",
            &admin_port.to_string(),
            "--allow-injection",
            "--local-only",
        ])
        .args(["--loglevel", "error"])
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let mut engine = Engine {
        which: Which::Rift,
        admin_port,
        child,
        pidfile: None,
    };
    wait_ready(&mut engine).await?;
    Ok(engine)
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
/// kills a connection must not poison the next request), a generous per-request timeout.
///
/// # Errors
/// When reqwest cannot build a client.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(30))
        .build()
}

/// A request already mapped to one engine's ports.
#[derive(Debug, Clone)]
pub struct Outgoing {
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
    let url = format!("http://127.0.0.1:{}{}", request.port, request.path);
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
