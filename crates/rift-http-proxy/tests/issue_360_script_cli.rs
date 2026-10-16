//! Integration test for issue #360: `rift script check` / `rift script run` invoked as the real
//! built binary (a bonus over the library-level tests in `src/script_cli.rs`, which cover the
//! check/run LOGIC directly). One happy path each, via `std::process::Command` against the
//! checked-in fixtures.

mod support;

use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// AC (issue #360): `rift script check` on a valid script exits 0 and prints OK.
#[test]
fn script_check_valid_fixture_exits_zero() {
    let output = Command::new(support::server_bin())
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

    let output = Command::new(support::server_bin())
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
    let output = Command::new(support::server_bin())
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
    let output = Command::new(support::server_bin())
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

fn run_script(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(support::server_bin())
        .args(["script", "run"])
        .args(args)
        .output()
        .expect("run rift script run");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// Issue #1506: `--flow-id` names the flow the script runs under: `ctx.flowId`, and the id the
// `--state` seed is read back under.
#[test]
fn script_run_flow_id_sets_ctx_flow_id() {
    let flow = fixture("flow-id.rhai");
    let (ok, stdout, stderr) = run_script(&[flow.to_str().expect("utf-8"), "--flow-id", "f9"]);
    assert!(ok, "stdout: {stdout}, stderr: {stderr}");
    assert!(stdout.contains("body=\"flow=f9\""), "stdout: {stdout}");
}

#[test]
fn script_run_flow_id_reads_the_seeded_state_under_that_flow() {
    let script = fixture("incr-state.rhai");
    let (ok, stdout, stderr) = run_script(&[
        script.to_str().expect("utf-8"),
        "--state",
        "n=5",
        "--flow-id",
        "f9",
    ]);
    assert!(ok, "stdout: {stdout}, stderr: {stderr}");
    assert!(stdout.contains("body=\"n=6\""), "stdout: {stdout}");
    assert!(stdout.contains("n = 6"), "stdout: {stdout}");
}

// Issue #1506: a file whose extension names no engine is refused until `--engine` says which.
#[test]
fn script_run_unknown_extension_needs_engine() {
    let script = fixture("js-body.txt");
    let script = script.to_str().expect("utf-8");
    let (ok, stdout, stderr) = run_script(&[script]);
    assert!(!ok, "expected non-zero exit, stdout: {stdout}");
    assert!(
        stderr.contains("cannot infer script engine"),
        "stderr: {stderr}"
    );
}

#[cfg(feature = "javascript")]
#[test]
fn script_run_engine_js_runs_a_file_with_no_engine_extension() {
    let script = fixture("js-body.txt");
    let (ok, stdout, stderr) = run_script(&[script.to_str().expect("utf-8"), "--engine", "js"]);
    assert!(ok, "stdout: {stdout}, stderr: {stderr}");
    assert!(stdout.contains("body=\"js-engine\""), "stdout: {stdout}");
}
