//! Issue #1603: a `--datadir` restart resumes each stub's response cycle where it stopped, as
//! Mountebank's does (`cli/datadirTest.js` "should load previously saved imposters until they are
//! deleted"). Rift wrote only the config to the datadir, so every cycle restarted at the first
//! response.
//!
//! These run the real binary: the positions are written behind and on the graceful stop a
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

/// Create an imposter on an assigned port (no fixed test port, see `test_port_uniqueness`).
fn post_imposter(server: &Server, responses: serde_json::Value) -> u16 {
    let body = serde_json::json!({"protocol": "http", "stubs": [{"responses": responses}]});
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

fn get(port: u16) -> String {
    let (body, code) = curl(&[&format!("http://127.0.0.1:{port}/")]);
    assert_eq!(code, "200", "imposter answers: {body}");
    body
}

fn bodies(labels: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        labels
            .iter()
            .map(|l| serde_json::json!({"is": {"body": l}}))
            .collect(),
    )
}

#[test]
fn a_graceful_restart_resumes_the_response_cycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path(), 1);
    let port = post_imposter(&server, bodies(&["first", "second"]));
    assert_eq!(get(port), "first");
    server.stop(libc::SIGINT);

    let server = spawn_server(dir.path(), 2);
    assert_eq!(get(port), "second", "the cycle resumes after the restart");
    assert_eq!(get(port), "first", "and wraps as before");
    server.stop(libc::SIGTERM);
}

#[test]
fn a_restart_resumes_inside_a_repeat_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path(), 1);
    let port = post_imposter(
        &server,
        serde_json::json!([
            {"is": {"body": "first"}, "repeat": 2},
            {"is": {"body": "second"}},
        ]),
    );
    assert_eq!(get(port), "first");
    server.stop(libc::SIGTERM);

    let server = spawn_server(dir.path(), 2);
    assert_eq!(get(port), "first", "the second of its two repeats");
    assert_eq!(get(port), "second");
    assert_eq!(get(port), "first");
    server.stop(libc::SIGTERM);
}

#[test]
fn deleting_the_imposter_deletes_its_positions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path(), 1);
    let port = post_imposter(&server, bodies(&["first", "second"]));
    assert_eq!(get(port), "first");
    // Within the flush period the file is written behind.
    let cursors = dir.path().join("data").join(format!("{port}.cursors"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cursors.exists() {
        assert!(Instant::now() < deadline, "{cursors:?} is written behind");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, code) = curl(&[
        "-X",
        "DELETE",
        &format!("http://127.0.0.1:{}/imposters/{port}", server.admin_port),
    ]);
    assert_eq!(code, "200");
    assert!(!cursors.exists(), "a delete removes {cursors:?}");
    server.stop(libc::SIGTERM);
    assert!(
        !cursors.exists(),
        "and the stop's flush does not bring it back"
    );
}
