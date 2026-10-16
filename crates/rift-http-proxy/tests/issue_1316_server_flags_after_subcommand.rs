//! Issue #1316: Mountebank's grammar is `mb start|restart|save|replay|stop <flags>`, but every Rift
//! server flag was top-level, so `rift start --port 2525 --configfile imposters.json` was refused
//! with "unexpected argument". This runs the real binary in exactly that form.
//!
//! Process-level because the parse, the dispatch and the bind all have to agree; the clap-level
//! cases live next to `Cli` in `server.rs`.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The port in the admin API's `listening on http://<addr>` line, if `line` is that line.
fn admin_port_in(line: &str) -> Option<u16> {
    if !line.contains("Admin API") {
        return None;
    }
    let addr = line.split("listening on http://").nth(1)?;
    addr.split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// The first `"port": N` in an admin response body.
fn first_port_in(response: &str) -> Option<u16> {
    let rest = response.split("\"port\":").nth(1)?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
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
    // One imposter, so the test proves `--configfile` after `start` is loaded, not just parsed.
    std::fs::write(
        &configfile,
        r#"{"imposters": [{"protocol": "http",
            "stubs": [{"responses": [{"is": {"body": "loaded-from-configfile"}}]}]}]}"#,
    )
    .expect("write configfile");

    let child = Command::new(support::server_bin())
        .args([
            "start",
            // Port 0 (read back from stdout) rather than a probed port, which another test can take.
            "--port",
            "0",
            "--metrics-port",
            "0",
            "--host",
            "127.0.0.1",
            "--configfile",
        ])
        .arg(&configfile)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rift start");
    let mut server = Server(child);
    let stdout = server.0.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    // Keeps draining after the port is found, so the child never blocks on a full pipe.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(port) = admin_port_in(&line) {
                let _ = tx.send(port);
            }
        }
    });
    let mut port = None;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if port.is_none() {
            port = rx.try_recv().ok();
        }
        if let Some(response) = port.and_then(|port| get(port, "/health")) {
            let status = response.lines().next().unwrap_or_default();
            assert!(status.contains("200"), "got: {status}");
            let port = port.expect("admin port");
            let listed = get(port, "/imposters").expect("GET /imposters");
            let imposter_port = first_port_in(&listed)
                .unwrap_or_else(|| panic!("the configfile's imposter is listed: {listed}"));
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
            "/health never answered (admin port {port:?})"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
