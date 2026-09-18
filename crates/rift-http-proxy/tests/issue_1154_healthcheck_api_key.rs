//! Issue #1154: `rift healthcheck` could not present an API key, so a container with `MB_APIKEY`
//! set — the configuration the images document for a locked-down deployment — reported unhealthy
//! forever: orchestrators restarted it, rolling deploys stalled.
//!
//! No new flag is needed, and adding one would be wrong: `api_key` is a top-level `Cli` field bound
//! to `MB_APIKEY`, so clap already fills it for every subcommand, and an rcfile `apiKey` is applied
//! before the healthcheck is dispatched. The probe already *held* the key and never sent it.
//!
//! These run the real binary, because the claim under test is about the process: that the key
//! reaches the probe from the environment and from the rcfile, with no flag on the subcommand.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

/// A keyed admin plane: `200` only when the request carries `authorization: <key>` exactly (the raw
/// token, which is what the real admin plane compares), `401` otherwise.
fn spawn_keyed_admin(key: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe target");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let authorized = head
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("authorization: {key}")));
            let response: &[u8] = if authorized {
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
            } else {
                b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            };
            let _ = stream.write_all(response);
            let _ = stream.flush();
        }
    });
    port
}

fn healthcheck(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(support::server_bin());
    cmd.args(args).env_remove("MB_APIKEY");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run healthcheck")
}

fn show(out: &Output) -> String {
    format!(
        "status={:?} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// The container case, exactly: MB_APIKEY in the environment, `rift healthcheck` with no flags.
#[test]
fn healthcheck_presents_mb_apikey_from_the_environment() {
    let port = spawn_keyed_admin("k");
    let port_s = port.to_string();
    let out = healthcheck(&["--port", &port_s, "healthcheck"], &[("MB_APIKEY", "k")]);
    assert!(
        out.status.success(),
        "a keyed server must be healthy to a probe with MB_APIKEY set: {}",
        show(&out)
    );
}

// Without the key the verdict is still unhealthy — and now it says why.
#[test]
fn healthcheck_without_a_key_is_unhealthy_and_names_the_remedy() {
    let port = spawn_keyed_admin("k");
    let port_s = port.to_string();
    let out = healthcheck(&["--port", &port_s, "healthcheck"], &[]);
    assert!(
        !out.status.success(),
        "no key must stay unhealthy: {}",
        show(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MB_APIKEY"),
        "the failure must point at the fix: {}",
        show(&out)
    );
}

// The same key from an rcfile, which is applied before the healthcheck dispatch (issue #1133).
#[test]
fn healthcheck_presents_an_rcfile_api_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = spawn_keyed_admin("k");
    let rcfile = write_rcfile(dir.path(), &format!(r#"{{"port": {port}, "apiKey": "k"}}"#));
    let out = healthcheck(
        &["--rcfile", rcfile.to_str().expect("utf-8"), "healthcheck"],
        &[],
    );
    assert!(
        out.status.success(),
        "an rcfile apiKey must reach the probe: {}",
        show(&out)
    );
}

/// A keyed admin plane that also serves the replayable imposter document `rift save` fetches.
fn spawn_keyed_admin_with_imposters(key: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind admin");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let authorized = head
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("authorization: {key}")));
            let body = r#"{"imposters":[]}"#;
            let response = if authorized {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_string()
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

// `rift save`'s half of the bug lived in the CLI wiring — `main.rs`'s `Save` arm never passed the
// key. The library-level test cannot see that; only the real binary can.
#[test]
fn save_presents_mb_apikey_from_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let savefile = dir.path().join("mb.json");
    let port = spawn_keyed_admin_with_imposters("k");
    let port_s = port.to_string();
    let out = healthcheck(
        &[
            "--port",
            &port_s,
            "save",
            "--savefile",
            savefile.to_str().expect("utf-8"),
        ],
        &[("MB_APIKEY", "k")],
    );
    assert!(
        out.status.success(),
        "rift save must present MB_APIKEY to a keyed server: {}",
        show(&out)
    );
    let saved = std::fs::read_to_string(&savefile).expect("savefile written");
    assert!(saved.contains("imposters"), "got: {saved}");
}

#[test]
fn save_without_a_key_fails_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let savefile = dir.path().join("mb.json");
    let port = spawn_keyed_admin_with_imposters("k");
    let port_s = port.to_string();
    let out = healthcheck(
        &[
            "--port",
            &port_s,
            "save",
            "--savefile",
            savefile.to_str().expect("utf-8"),
        ],
        &[],
    );
    assert!(!out.status.success(), "no key must fail: {}", show(&out));
    assert!(!savefile.exists(), "a refused save must write nothing");
}

fn write_rcfile(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("rift.rc");
    std::fs::write(&path, body).expect("write rcfile");
    path
}
