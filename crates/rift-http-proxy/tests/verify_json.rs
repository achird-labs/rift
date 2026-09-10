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
        "port": 22655, "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/ping" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "pong" } }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(cfg).await.expect("create imposter");

    let addr = "127.0.0.1:22658".parse().unwrap();
    tokio::spawn(rift_http_proxy::admin_api::AdminApiServer::new(addr, manager, None).run());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let out = Command::new(BIN)
        .args(["--admin-url", "http://127.0.0.1:22658", "-o", "json"])
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
