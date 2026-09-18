//! Issue #1151: `RIFT_STRICT_BEHAVIORS` must reach `defaultResponse` as well as the stub `is` path.
//!
//! The default-response decode used to read no strict flag at all — the local that held it was
//! scoped to the matched-stub block. The per-imposter `strictBehaviors` case is covered in
//! `issue_1151_default_response_binary.rs`; this is the process-wide spelling.
//!
//! This needs its own process: `RIFT_STRICT_BEHAVIORS` is read once into a `OnceLock`, so it must
//! be set before the first request in this binary, and a file whose other tests rely on the
//! lenient default (`issue_375_strict_behaviors.rs`) cannot host it. No test here sets the
//! per-imposter flag, so a 500 can only have come from the environment variable.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

fn enable_strict_behaviors() {
    // SAFETY (env mutation, not memory): set before any request is served, so every test in this
    // binary reads the same (strict) value — the own-process pattern of
    // issue_359_response_templating_debug.rs.
    unsafe { std::env::set_var("RIFT_STRICT_BEHAVIORS", "1") };
}

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn the_env_var_makes_an_undecodable_binary_default_response_a_500() {
    enable_strict_behaviors();
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21920, "protocol": "http", "stubs": [],
            "defaultResponse": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" }
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:21920/no-stub-matches-this")
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        500,
        "RIFT_STRICT_BEHAVIORS must fail loud on the default response, as it does on a stub"
    );
    assert!(resp.headers().contains_key("x-rift-binary-error"));
    assert_eq!(
        resp.headers()
            .get("x-rift-default-response")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "the strict 500 must still say it came from the default response"
    );
    let _ = manager.delete_imposter(21920).await;
}

// The same process-wide flag on the stub `is` path, which had no end-to-end env-var test either —
// only the pure parser was unit-tested. Cheap to pin here, since this binary already owns the flag.
#[tokio::test]
async fn the_env_var_makes_an_undecodable_binary_is_body_a_500() {
    enable_strict_behaviors();
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21921, "protocol": "http",
            "stubs": [{ "responses": [{ "is": {
                "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary"
            } }] }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:21921/x")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 500);
    assert!(resp.headers().contains_key("x-rift-binary-error"));
    let _ = manager.delete_imposter(21921).await;
}
