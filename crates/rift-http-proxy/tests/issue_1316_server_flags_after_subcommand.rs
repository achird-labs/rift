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

fn stub(path: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "predicates": [{"equals": {"path": path}}],
        "responses": [{"is": {"statusCode": 200, "body": body}}]
    })
}

async fn text(client: &reqwest::Client, url: String) -> (u16, String) {
    let response = client.get(&url).send().await.expect(&url);
    (
        response.status().as_u16(),
        response.text().await.expect("body"),
    )
}

// Issue #1506 (ported from rift-conformance's RiftCliLifecycleSpec): `save` writes the live stubs
// and `replay --configfile` serves that file, end to end through the real binary.
#[tokio::test]
async fn replay_serves_the_file_that_save_wrote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let saved = dir.path().join("saved.json");
    let saved_arg = saved.to_str().expect("utf-8 path");
    let client = reqwest::Client::new();

    let (mut first, admin) = support::spawn_server(&["--host", "127.0.0.1"]).await;
    let created = client
        .post(format!("http://127.0.0.1:{admin}/imposters"))
        .json(&serde_json::json!({"protocol": "http", "stubs": [stub("/first", "first")]}))
        .send()
        .await
        .expect("create imposter");
    assert_eq!(created.status().as_u16(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("assigned port");
    let added = client
        .post(format!("http://127.0.0.1:{admin}/imposters/{port}/stubs"))
        .json(&serde_json::json!({"stub": stub("/added", "added-live")}))
        .send()
        .await
        .expect("add stub");
    assert!(added.status().is_success(), "add stub: {}", added.status());

    let admin_arg = admin.to_string();
    let save = Command::new(support::server_bin())
        .args(["--port", &admin_arg, "save", "--savefile", saved_arg])
        .output()
        .expect("run rift save");
    assert_eq!(
        save.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&save.stderr)
    );
    first.kill().await.expect("stop the first server");

    let (mut replayed, admin) =
        support::spawn_server(&["--host", "127.0.0.1", "replay", "--configfile", saved_arg]).await;
    // The replay engine's own admin view is deterministic even if another process co-binds the port.
    let (status, listed) = text(
        &client,
        format!("http://127.0.0.1:{admin}/imposters/{port}"),
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    assert!(listed.contains("\"/first\""), "got: {listed}");
    assert!(listed.contains("added-live"), "got: {listed}");
    assert_eq!(
        text(&client, format!("http://127.0.0.1:{port}/first")).await,
        (200, "first".to_string())
    );
    assert_eq!(
        text(&client, format!("http://127.0.0.1:{port}/added")).await,
        (200, "added-live".to_string())
    );
    replayed.kill().await.expect("stop the replay server");
}

// Issue #1506: a bare `replay` has nothing to serve and says so (exit 1, not a parse error).
#[test]
fn replay_without_a_configfile_exits_1() {
    let output = Command::new(support::server_bin())
        .args(["--port", "0", "replay"])
        .output()
        .expect("run rift replay");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rift replay needs --configfile <file>"),
        "stderr: {stderr}"
    );
}

// Issue #1506: exit 2 is "the command line could not be parsed" (clap), distinct from 1.
#[test]
fn an_unknown_flag_is_a_parse_error_with_exit_2() {
    let output = Command::new(support::server_bin())
        .arg("--no-such-flag")
        .output()
        .expect("run rift");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument '--no-such-flag'"),
        "stderr: {stderr}"
    );
}
