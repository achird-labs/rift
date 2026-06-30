//! Live integration tests for `rift-verify --verify-dynamic` (issue #251).
//!
//! Each test stands up an in-process `AdminApiServer` with dynamic imposters, then runs the
//! compiled `rift-verify` binary as a subprocess pointing at it. The binary stands up its own
//! embedded mock upstream, recreates throwaway imposters on free ports, and asserts the dynamic
//! behaviors end-to-end. Self-contained (no external server), so they run in the default suite.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-verify");

async fn start_admin(port: u16, manager: Arc<ImposterManager>) -> String {
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(addr, manager, None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    format!("http://127.0.0.1:{port}")
}

async fn create(manager: &ImposterManager, config: serde_json::Value) {
    let config = serde_json::from_value(config).expect("valid imposter config");
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
}

async fn run_verify(admin: &str) -> std::process::Output {
    Command::new(BIN)
        .args(["--admin-url", admin, "--skip-dynamic", "--verify-dynamic"])
        .output()
        .await
        .expect("run rift-verify")
}

#[tokio::test]
async fn verify_dynamic_asserts_proxy_verify_and_fault() {
    let manager = Arc::new(ImposterManager::new());

    // Mechanism 2: a repeat-cycling stub with a `_verify` sequence (clean state on a fresh imposter).
    create(
        &manager,
        serde_json::json!({
            "port": 19861, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/r" } }],
                "responses": [
                    { "is": { "statusCode": 503 }, "_behaviors": { "repeat": 2 } },
                    { "is": { "statusCode": 200 } }
                ],
                "_verify": { "sequence": [
                    { "request": { "path": "/r" }, "expect": { "status": 503 } },
                    { "request": { "path": "/r" }, "expect": { "status": 503 } },
                    { "request": { "path": "/r" }, "expect": { "status": 200 } },
                    { "request": { "path": "/r" }, "expect": { "status": 503 } }
                ]}
            }]
        }),
    )
    .await;

    // Mechanism 1: a proxyOnce stub with predicateGenerators (rift-verify repoints `to` at its mock).
    create(
        &manager,
        serde_json::json!({
            "port": 19862, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": {
                "to": "http://127.0.0.1:1", "mode": "proxyOnce",
                "predicateGenerators": [{ "matches": { "method": true, "path": true } }]
            }}]}]
        }),
    )
    .await;

    // Mechanism 3: a deterministic error fault.
    create(
        &manager,
        serde_json::json!({
            "port": 19863, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/err" } }],
                "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "fault": { "error": { "probability": 1.0, "status": 503, "body": "down" } } } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12671, manager).await;
    let out = run_verify(&admin).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "rift-verify should exit 0; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // AC6 _verify sequence, AC7 proxy sentinel + record, AC8 fault status.
    assert!(
        stdout.contains("_verify["),
        "missing _verify checks:\n{stdout}"
    );
    assert!(
        stdout.contains("proxied sentinel"),
        "missing proxy check:\n{stdout}"
    );
    assert!(
        stdout.contains("records stub (+1)"),
        "missing proxy record check:\n{stdout}"
    );
    assert!(
        stdout.contains("error status 503"),
        "missing fault check:\n{stdout}"
    );
    assert!(
        !stdout.contains("FAIL"),
        "no dynamic check should fail:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_dynamic_fails_on_wrong_expectation() {
    let manager = Arc::new(ImposterManager::new());
    // A `_verify` step asserts the wrong status — the verifier must FAIL (exit 1).
    create(
        &manager,
        serde_json::json!({
            "port": 19871, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/ok" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "ok" } }],
                "_verify": { "sequence": [
                    { "request": { "path": "/ok" }, "expect": { "status": 418 } }
                ]}
            }]
        }),
    )
    .await;

    let admin = start_admin(12681, manager).await;
    let out = run_verify(&admin).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "should exit non-zero on a failed expectation:\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "should report a FAIL:\n{stdout}");
}

#[tokio::test]
async fn dynamic_skipped_without_flag() {
    // The opt-in contract: WITHOUT --verify-dynamic, dynamic stubs are skipped (no assertion,
    // no mock upstream, no throwaway imposters) — the original safe behavior is preserved.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19881, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": {
                "to": "http://127.0.0.1:1", "mode": "proxyOnce",
                "predicateGenerators": [{ "matches": { "path": true } }]
            }}]}]
        }),
    )
    .await;

    let admin = start_admin(12691, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin, "--skip-dynamic"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "should exit 0:\n{stdout}");
    assert!(
        !stdout.contains("Dynamic assertions"),
        "no dynamic section should appear without --verify-dynamic:\n{stdout}"
    );
    assert!(
        !stdout.contains("proxied sentinel"),
        "no proxy assertion should run without the flag:\n{stdout}"
    );
}
