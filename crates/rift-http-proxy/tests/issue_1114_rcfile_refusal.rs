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

// Issue #1574: Mountebank warns about a `--rcfile` that does not exist and starts without it, so
// Rift does too. (#1114 had made it fatal, alongside the wrong-typed file it was really about.)
#[test]
fn a_missing_rcfile_warns_and_the_server_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("does-not-exist.rc");

    let mut child = Command::new(support::server_bin())
        .arg("--rcfile")
        .arg(&rcfile)
        .args(["--host", "127.0.0.1", "--port", "0", "--metrics-port", "0"])
        .arg("--pidfile")
        .arg(dir.path().join("rift.pid"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the server");
    // Still running after the window an aborting startup needs to exit: it started.
    std::thread::sleep(Duration::from_secs(3));
    let exited = child.try_wait().expect("poll the server");
    child.kill().ok();
    let output = child.wait_with_output().expect("collect output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(exited.is_none(), "the server exited; stderr: {stderr}");
    assert!(
        stderr.contains("not found; starting without it")
            && stderr.contains(&rcfile.display().to_string()),
        "the warning names the file: {stderr}"
    );
}

// Only a path that does not exist is tolerated: a file that is there but is not JSON still aborts.
#[test]
fn an_rcfile_that_is_not_json_still_aborts_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("rift.rc");
    std::fs::write(&rcfile, "debug: true").expect("write rcfile");

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

// A directory exists, so it is not "missing": reading it fails and that stays fatal.
#[test]
fn an_rcfile_path_that_is_a_directory_still_aborts_startup() {
    let dir = tempfile::tempdir().expect("tempdir");

    let output = start_with_rcfile(
        dir.path(),
        &dir.path().join("rift.pid"),
        Duration::from_secs(20),
    );
    assert!(
        !output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
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
    // A `stop` with no PID file succeeds (issue #1559), so a zero exit is the proof.
    assert!(
        output.status.success(),
        "a valid rcfile must let the command run past the rcfile step: {stderr}"
    );
}
