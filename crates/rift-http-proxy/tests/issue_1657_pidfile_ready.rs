//! Issue #1657: `--pidfile` was written at the top of the serving path, before the runtime existed,
//! so it appeared before the admin API or any `--configfile` imposter was bound. Mountebank's
//! `bin/mb` writes it last, "for build plugins that need to wait for mb to be fully initialized",
//! and its own CLI tests do exactly that: wait for the file, then hit the imposters. Against Rift
//! they raced a connection-refused.
//!
//! These run the real binary, because the order is a property of the process.

mod support;

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the server may take to come up (or fail) before the test calls it hung.
const STARTUP_BUDGET: Duration = Duration::from_secs(20);

/// `N` distinct ports for a config file that must name them before the server boots. Every probe is
/// held until all are chosen, so none repeats, and probes `0.0.0.0`, the address imposters bind.
fn free_ports<const N: usize>() -> [u16; N] {
    let listeners: Vec<std::net::TcpListener> = (0..N)
        .map(|_| std::net::TcpListener::bind("0.0.0.0:0").expect("bind an ephemeral port"))
        .collect();
    std::array::from_fn(|i| listeners[i].local_addr().expect("local addr").port())
}

/// One `GET /` on `port`, attempted exactly once: the point is that no retry is needed.
fn get_once(port: u16) -> std::io::Result<String> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn spawn(args: &[&str]) -> std::process::Child {
    Command::new(support::server_bin())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rift")
}

fn utf8(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

#[test]
fn pidfile_appears_only_once_startup_imposters_serve() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("rift.pid");
    let config = dir.path().join("imposters.json");
    // Several imposters widen the window the old order left open: each is bound in turn.
    let ports = free_ports::<8>();
    let imposters: Vec<String> = ports
        .iter()
        .map(|port| {
            format!(
                r#"{{"port":{port},"protocol":"http",
                    "stubs":[{{"responses":[{{"is":{{"statusCode":200,"body":"ready-{port}"}}}}]}}]}}"#
            )
        })
        .collect();
    std::fs::write(
        &config,
        format!(r#"{{"imposters":[{}]}}"#, imposters.join(",")),
    )
    .expect("write");

    let mut child = spawn(&[
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--pidfile",
        utf8(&pidfile),
        "--configfile",
        utf8(&config),
    ]);

    let deadline = Instant::now() + STARTUP_BUDGET;
    while !pidfile.exists() {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("rift exited ({status}) before writing its PID file");
        }
        assert!(
            Instant::now() < deadline,
            "no PID file within {STARTUP_BUDGET:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // The file is there: every startup imposter must answer now, on the first attempt.
    let results: Vec<(u16, std::io::Result<String>)> =
        ports.iter().map(|&port| (port, get_once(port))).collect();
    let pid = std::fs::read_to_string(&pidfile).expect("read PID file");
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        pid.trim(),
        child.id().to_string(),
        "the PID file names the server"
    );
    for (port, result) in results {
        let response = result.unwrap_or_else(|e| {
            panic!("imposter :{port} was not serving when the PID file appeared: {e}")
        });
        assert!(
            response.starts_with("HTTP/1.1 200") && response.ends_with(&format!("ready-{port}")),
            "imposter :{port} answered unexpectedly: {response}"
        );
    }
}

#[test]
fn failed_startup_writes_no_pidfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("rift.pid");
    let config = dir.path().join("imposters.json");
    // An `inject` response without `--allowInjection` aborts startup.
    std::fs::write(
        &config,
        r#"{"imposters":[{"protocol":"http","stubs":[{"responses":[
            {"inject":"function (config) { return { statusCode: 200 }; }"}]}]}]}"#,
    )
    .expect("write");

    let mut child = spawn(&[
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--pidfile",
        utf8(&pidfile),
        "--configfile",
        utf8(&config),
    ]);

    // Watched for the whole run, not only at the end: the old order wrote the file and then removed
    // it on the way out, so a check after exit alone would pass on it.
    let deadline = Instant::now() + STARTUP_BUDGET;
    let mut seen = false;
    let status = loop {
        seen |= pidfile.exists();
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a refused startup must exit, not keep serving");
        }
        std::thread::sleep(Duration::from_micros(200));
    };
    seen |= pidfile.exists();

    assert!(!status.success(), "a refused startup must exit non-zero");
    assert!(
        !seen,
        "a startup that failed wrote a PID file; tooling waiting on it would think the server ready"
    );
}
