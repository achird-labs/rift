//! Integration test for issue #360: `rift script check` / `rift script run` invoked as the real
//! built binary (a bonus over the library-level tests in `src/script_cli.rs`, which cover the
//! check/run LOGIC directly). One happy path each, via `std::process::Command` against the
//! checked-in fixtures.

use std::path::PathBuf;
use std::process::Command;

fn rift_bin() -> PathBuf {
    // The binary target is named `rift-http-proxy` (no `[[bin]] name = "rift"` override in
    // Cargo.toml — the CLI itself is invoked as `rift` only once packaged/aliased downstream).
    PathBuf::from(env!("CARGO_BIN_EXE_rift-http-proxy"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// AC (issue #360): `rift script check` on a valid script exits 0 and prints OK.
#[test]
fn script_check_valid_fixture_exits_zero() {
    let output = Command::new(rift_bin())
        .args(["script", "check"])
        .arg(fixture("fail-twice.rhai"))
        .output()
        .expect("run rift script check");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("OK"), "stdout: {stdout}");
}

// AC (issue #360): a syntax-valid script whose only function is misnamed fails `script check`,
// naming the expected entrypoint, and exits non-zero.
#[test]
fn script_check_misnamed_entrypoint_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.rhai");
    std::fs::write(&path, "fn respnod(ctx) { pass() }").expect("write fixture");

    let output = Command::new(rift_bin())
        .args(["script", "check"])
        .arg(&path)
        .output()
        .expect("run rift script check");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "expected non-zero exit, stdout: {stdout}"
    );
    assert!(
        stdout.contains("respond"),
        "error must name the expected entrypoint: {stdout}"
    );
}

// AC (issue #360): `rift script run` on the fail-twice fixture with `--state attempts=2` prints
// the 200 (pass) branch, with no server running.
#[test]
fn script_run_fail_twice_attempts_2_passes() {
    let output = Command::new(rift_bin())
        .args(["script", "run"])
        .arg(fixture("fail-twice.rhai"))
        .args(["--request"])
        .arg(fixture("get-resource.json"))
        .args(["--state", "attempts=2"])
        .output()
        .expect("run rift script run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("pass()"), "stdout: {stdout}");
}

#[test]
fn script_run_fail_twice_attempts_1_fails_with_503() {
    let output = Command::new(rift_bin())
        .args(["script", "run"])
        .arg(fixture("fail-twice.rhai"))
        .args(["--state", "attempts=1"])
        .output()
        .expect("run rift script run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("http(503)"), "stdout: {stdout}");
    assert!(stdout.contains("attempts"), "state dump missing: {stdout}");
}
