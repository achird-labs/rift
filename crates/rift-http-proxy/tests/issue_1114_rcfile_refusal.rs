//! Issue #1114: an `--rcfile` that `apply_rcfile_defaults` refused was only warned about, and the
//! server started with none of its keys. A mistyped `"requireAdminAuth": "true"` therefore served
//! the admin plane off-host with no authentication. These run the real binary, because the decision
//! lives in `main.rs`, not in the library.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run the server with `--rcfile`, on ephemeral ports, and give up after `limit`. Before the fix a
/// refused rcfile let the server start and keep running, so a hang is the failure being tested for.
fn start_with_rcfile(rcfile: &Path, pidfile: &Path, limit: Duration) -> Output {
    let mut child = Command::new(support::server_bin())
        .arg("--rcfile")
        .arg(rcfile)
        .args(["--host", "127.0.0.1", "--port", "0", "--metrics-port", "0"])
        .arg("--pidfile")
        .arg(pidfile)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll the server").is_some() {
            return child.wait_with_output().expect("collect output");
        }
        if started.elapsed() > limit {
            child.kill().ok();
            let output = child.wait_with_output().expect("collect output");
            panic!(
                "the server kept running with a refused rcfile; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_non_boolean_require_admin_auth_aborts_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("rift.rc");
    std::fs::write(&rcfile, r#"{"requireAdminAuth": "true"}"#).expect("write rcfile");

    let output = start_with_rcfile(
        &rcfile,
        &dir.path().join("rift.pid"),
        Duration::from_secs(20),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("requireAdminAuth"),
        "the error names the key: {stderr}"
    );
    assert!(
        stderr.contains(&rcfile.display().to_string()),
        "the error names the file: {stderr}"
    );
}

#[test]
fn a_missing_rcfile_aborts_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("does-not-exist.rc");

    let output = start_with_rcfile(
        &rcfile,
        &dir.path().join("rift.pid"),
        Duration::from_secs(20),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(&rcfile.display().to_string()),
        "the error names the file: {stderr}"
    );
}

// The unsupported-key warning was emitted through `tracing` before the subscriber was installed,
// so it never reached the terminal. `stop` runs after the rcfile is applied and exits at once on a
// missing PID file, which makes it a cheap way to see what the rcfile step printed.
#[test]
fn an_unsupported_rcfile_key_is_reported_on_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("rift.rc");
    std::fs::write(&rcfile, r#"{"bogusKey": 1}"#).expect("write rcfile");

    let output = Command::new(support::server_bin())
        .arg("--rcfile")
        .arg(&rcfile)
        .arg("--pidfile")
        .arg(dir.path().join("missing.pid"))
        .arg("stop")
        .output()
        .expect("run rift stop");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported key 'bogusKey'"),
        "the ignored key must be visible: {stderr}"
    );
    assert!(
        stderr.contains("PID file not found"),
        "a valid rcfile must let the command run past the rcfile step: {stderr}"
    );
}
