//! Issue #1316: Mountebank's grammar is `mb start|restart|save|replay|stop <flags>`, but every Rift
//! server flag was top-level, so `rift start --port 2525 --configfile imposters.json` was refused
//! with "unexpected argument". This runs the real binary in exactly that form.
//!
//! Process-level because the parse, the dispatch and the bind all have to agree; the clap-level
//! cases live next to `Cli` in `server.rs`.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Reserve a port, then release it, so the child can bind it.
fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    probe.local_addr().expect("probe addr").port()
}

/// Kills the child on every exit path, panics included.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `GET <path>` over a raw socket; `Some(whole response)` once the server answers.
fn get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

// The Mountebank form, run for real: server flags AFTER the subcommand.
#[test]
fn rift_start_with_flags_after_the_subcommand_boots_and_serves_health() {
    let dir = tempfile::tempdir().expect("tempdir");
    let configfile = dir.path().join("imposters.json");
    let port = free_port();
    let metrics_port = free_port();
    // One imposter, so the test proves `--configfile` after `start` is loaded, not just parsed.
    let imposter_port = free_port();
    std::fs::write(
        &configfile,
        format!(
            r#"{{"imposters": [{{"port": {imposter_port}, "protocol": "http",
                "stubs": [{{"responses": [{{"is": {{"body": "loaded-from-configfile"}}}}]}}]}}]}}"#
        ),
    )
    .expect("write configfile");

    let child = Command::new(support::server_bin())
        .args([
            "start",
            "--port",
            &port.to_string(),
            "--metrics-port",
            &metrics_port.to_string(),
            "--host",
            "127.0.0.1",
            "--configfile",
        ])
        .arg(&configfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rift start");
    let mut server = Server(child);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(response) = get(port, "/health") {
            let status = response.lines().next().unwrap_or_default();
            assert!(status.contains("200"), "got: {status}");
            let served = get(imposter_port, "/").expect("the configfile's imposter answers");
            assert!(served.contains("loaded-from-configfile"), "got: {served}");
            return;
        }
        if let Some(exit) = server.0.try_wait().expect("try_wait") {
            let mut stderr = String::new();
            if let Some(mut pipe) = server.0.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("rift start exited early ({exit}): {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "/health never answered on {port}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
