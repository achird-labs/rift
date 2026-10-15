//! Issue #347: `rift-verify -o json` emits a pure-JSON summary on stdout (no banner/ANSI), with
//! the fields {imposters, stubs, tests, passed, failed, skipped}. Reuses the embedded-server
//! harness pattern from verify_dynamic.rs.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-verify");

#[tokio::test]
async fn verify_json_output_has_summary_fields() {
    let manager = Arc::new(ImposterManager::new());
    let cfg = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/ping" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "pong" } }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(cfg).await.expect("create imposter");

    let running = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        manager,
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let admin_url = format!("http://{}", running.local_addr());

    let out = Command::new(BIN)
        .args(["--admin-url", admin_url.as_str(), "-o", "json"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8(out.stdout).expect("utf8");

    assert!(
        !stdout.contains('\x1b'),
        "json-mode stdout must contain no ANSI escapes, got: {stdout:?}"
    );
    assert!(
        !stdout.contains("Verification Summary"),
        "json-mode stdout must not carry the human summary banner"
    );

    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout parses as a single JSON object");
    for k in ["imposters", "stubs", "tests", "passed", "failed", "skipped"] {
        assert!(
            v.get(k).and_then(serde_json::Value::as_u64).is_some(),
            "json summary must carry numeric field `{k}`, got: {v}"
        );
    }
    assert_eq!(v["imposters"].as_u64(), Some(1), "one imposter verified");
    assert_eq!(
        v["failed"].as_u64(),
        Some(0),
        "the static stub verifies cleanly"
    );
}

/// Issue #1380: a stub that predicates on a method outside GET/POST/PUT/DELETE/PATCH/HEAD is
/// probed with that method. It used to be probed with GET and reported as a mismatch.
#[tokio::test]
async fn verify_probes_an_options_stub_with_options() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let manager = Arc::new(ImposterManager::new());
    let cfg = serde_json::from_value(serde_json::json!({
        "port": port, "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "method": "OPTIONS", "path": "/opt" } }],
            "responses": [{ "is": { "statusCode": 204 } }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(cfg).await.expect("create imposter");

    let running = rift_http_proxy::admin_api::AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        manager,
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let admin_url = format!("http://{}", running.local_addr());

    let out = Command::new(BIN)
        .args(["--admin-url", admin_url.as_str(), "-o", "json"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout parses as a single JSON object");
    assert_eq!(v["tests"].as_u64(), Some(1), "one probe: {v}");
    assert_eq!(v["failed"].as_u64(), Some(0), "OPTIONS stub verifies: {v}");
    assert_eq!(v["passed"].as_u64(), Some(1), "{v}");
}
