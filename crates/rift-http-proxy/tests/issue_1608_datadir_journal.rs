//! Issue #1608: with `--datadir`, recorded requests survive a restart, as Mountebank's do — its
//! datadir is the journal's live store. Rift kept them in memory only, so a restart lost them.
//! `numberOfRequests` still starts over, as Mountebank's in-process counter does.
//!
//! These run the real binary: the journal is written behind and on the graceful stop a
//! `SIGINT`/`SIGTERM` triggers, which is a property of the process.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    admin_port: u16,
    log: PathBuf,
}

/// The admin port in the server's `listening on http://<addr>` line, once `--log` holds it.
fn admin_port_in(log: &str) -> Option<u16> {
    let line = log
        .lines()
        .find(|l| l.contains("Admin API") && l.contains("listening on http://"))?;
    let addr = line.split("listening on http://").nth(1)?;
    addr.split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

fn spawn_server(dir: &Path, run: usize) -> Server {
    let datadir = dir.join("data");
    let log = dir.join(format!("rift-{run}.log"));
    let pidfile = dir.join(format!("rift-{run}.pid"));
    let mut command = Command::new(support::server_bin());
    command
        .args([
            "--port",
            "0",
            "--host",
            "127.0.0.1",
            "--metrics-port",
            "0",
            "--pidfile",
            pidfile.to_str().expect("utf-8"),
            "--datadir",
            datadir.to_str().expect("utf-8"),
            "--log",
            log.to_str().expect("utf-8"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A test harness can run with SIGINT blocked, and a signal mask survives exec: start each
    // server with a clean one, as issue_1155_signal_shutdown.rs does.
    // SAFETY: `pre_exec` runs in the forked child before exec; sigemptyset/sigprocmask are
    // async-signal-safe and touch only the child's own mask.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            if libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut server = Server {
        child: command.spawn().expect("spawn rift"),
        admin_port: 0,
        log,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = server.child.try_wait().expect("try_wait") {
            panic!(
                "rift exited during startup with {status:?}: {}",
                server.log()
            );
        }
        if server.admin_port == 0 {
            server.admin_port = admin_port_in(&server.log()).unwrap_or(0);
        }
        if server.admin_port != 0
            && std::net::TcpStream::connect(("127.0.0.1", server.admin_port)).is_ok()
            && pidfile.exists()
        {
            return server;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("rift did not come up: {}", server.log());
}

impl Server {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Stop with `sig` and wait for the exit, which must be the graceful one.
    fn stop(mut self, sig: libc::c_int) {
        let pid = libc::pid_t::try_from(self.child.id()).expect("pid fits pid_t");
        // SAFETY: sending a signal to a child this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(pid, sig) }, 0, "kill({pid}, {sig})");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                assert_eq!(status.code(), Some(0), "graceful exit: {}", self.log());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "rift did not exit: {}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// `curl` `args`, returning (body, status).
fn curl(args: &[&str]) -> (String, String) {
    let out = Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl");
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = out.rsplit_once('\n').expect("status line");
    (body.to_string(), code.to_string())
}

/// Create a recording imposter on an assigned port (no fixed test port, see
/// `test_port_uniqueness`).
fn post_imposter(server: &Server) -> u16 {
    let body = serde_json::json!({"protocol": "http", "recordRequests": true, "stubs": []});
    let (json, code) = curl(&[
        "-X",
        "POST",
        "-H",
        "content-type: application/json",
        "--data",
        &body.to_string(),
        &format!("http://127.0.0.1:{}/imposters", server.admin_port),
    ]);
    assert_eq!(code, "201", "imposter creation must succeed: {json}");
    let created: serde_json::Value = serde_json::from_str(&json).expect("201 body");
    created["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("assigned port")
}

fn imposter(server: &Server, port: u16) -> serde_json::Value {
    let (json, code) = curl(&[&format!(
        "http://127.0.0.1:{}/imposters/{port}",
        server.admin_port
    )]);
    assert_eq!(code, "200", "{json}");
    serde_json::from_str(&json).expect("imposter JSON")
}

fn requests(imposter: &serde_json::Value) -> Vec<(String, String, Option<String>)> {
    imposter["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|r| {
            (
                r["method"].as_str().expect("method").to_string(),
                r["path"].as_str().expect("path").to_string(),
                r["body"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn timestamps(imposter: &serde_json::Value) -> Vec<String> {
    imposter["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|r| r["timestamp"].as_str().expect("timestamp").to_string())
        .collect()
}

fn send(port: u16, args: &[&str]) {
    let url = format!("http://127.0.0.1:{port}{}", args[0]);
    let mut all: Vec<&str> = args[1..].to_vec();
    all.push(&url);
    let (_, code) = curl(&all);
    assert_eq!(code, "200", "the imposter answers {url}");
}

fn row(method: &str, path: &str, body: Option<&str>) -> (String, String, Option<String>) {
    (method.into(), path.into(), body.map(str::to_string))
}

#[test]
fn recorded_requests_survive_a_graceful_restart_and_a_clear_survives_one_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path(), 1);
    let port = post_imposter(&server);
    send(port, &["/a"]);
    send(port, &["/b", "-X", "POST", "--data", "hello"]);
    let before = timestamps(&imposter(&server, port));
    server.stop(libc::SIGINT);

    let server = spawn_server(dir.path(), 2);
    send(port, &["/c"]);
    let restarted = imposter(&server, port);
    assert_eq!(
        requests(&restarted),
        [
            row("GET", "/a", Some("")),
            row("POST", "/b", Some("hello")),
            row("GET", "/c", Some(""))
        ],
        "the earlier requests come back, in order: {restarted}"
    );
    assert_eq!(
        &timestamps(&restarted)[..2],
        &before[..],
        "with their timestamps"
    );
    assert_eq!(
        restarted["numberOfRequests"], 1,
        "the count starts over, as Mountebank's does"
    );

    let (_, code) = curl(&[
        "-X",
        "DELETE",
        &format!(
            "http://127.0.0.1:{}/imposters/{port}/savedRequests",
            server.admin_port
        ),
    ]);
    assert_eq!(code, "200");
    let file = dir
        .path()
        .join("data")
        .join(format!("{port}.requests.jsonl"));
    assert!(!file.exists(), "the clear removes {file:?}");
    server.stop(libc::SIGTERM);

    let server = spawn_server(dir.path(), 3);
    assert!(
        requests(&imposter(&server, port)).is_empty(),
        "a cleared journal stays cleared"
    );
    assert!(!file.exists());
    server.stop(libc::SIGTERM);
}

#[test]
fn deleting_the_imposter_deletes_its_recorded_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path(), 1);
    let port = post_imposter(&server);
    send(port, &["/a"]);
    let file = dir
        .path()
        .join("data")
        .join(format!("{port}.requests.jsonl"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !file.exists() {
        assert!(Instant::now() < deadline, "{file:?} is written behind");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, code) = curl(&[
        "-X",
        "DELETE",
        &format!("http://127.0.0.1:{}/imposters/{port}", server.admin_port),
    ]);
    assert_eq!(code, "200");
    assert!(!file.exists(), "a delete removes {file:?}");
    server.stop(libc::SIGTERM);
    assert!(
        !file.exists(),
        "and the stop's flush does not bring it back"
    );
}
