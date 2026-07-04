//! Issue #375 gate: opt-in strict mode. When `strictBehaviors: true` is set on an imposter, a
//! requested response behavior that FAILS returns a 500 (still carrying the #323
//! `x-rift-<behavior>-error` signal header) instead of serving the fallback body. The default
//! (flag absent) is unchanged — lenient + header, preserving #269's keep-the-body contract.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

// AC1: strict + failing decorate → 500 with x-rift-decorate-error, fallback body NOT served.
#[tokio::test]
async fn strict_decorate_failure_returns_500() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19951, "protocol": "http", "strictBehaviors": true, "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "decorate": "function (request, response) { throw new Error('boom'); }" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19951/x")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    let has_header = resp.headers().contains_key("x-rift-decorate-error");
    let body = resp.text().await.expect("body");
    assert_eq!(
        status, 500,
        "strict decorate failure must fail loud with 500"
    );
    assert!(
        has_header,
        "the 500 must still carry x-rift-decorate-error so the cause is visible"
    );
    assert_ne!(
        body, "original",
        "the fallback body must NOT be served in strict mode"
    );
    let _ = manager.delete_imposter(19951).await;
}

// AC2: strict + failing shellTransform → 500 with x-rift-shelltransform-error.
#[tokio::test]
async fn strict_shell_transform_failure_returns_500() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19952, "protocol": "http", "strictBehaviors": true, "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "shellTransform": "exit 1" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19952/x")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    let has_header = resp.headers().contains_key("x-rift-shelltransform-error");
    let body = resp.text().await.expect("body");
    assert_eq!(
        status, 500,
        "strict shellTransform failure must fail loud with 500"
    );
    assert!(
        has_header,
        "the 500 must still carry x-rift-shelltransform-error"
    );
    assert_ne!(
        body, "original",
        "the fallback body must NOT be served in strict mode"
    );
    let _ = manager.delete_imposter(19952).await;
}

// AC3: strict + invalid binary base64 → 500 with x-rift-binary-error.
#[tokio::test]
async fn strict_binary_decode_failure_returns_500() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19953, "protocol": "http", "strictBehaviors": true, "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19953/x")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    assert_eq!(
        status, 500,
        "strict binary decode failure must fail loud with 500"
    );
    assert!(
        resp.headers().contains_key("x-rift-binary-error"),
        "the 500 must still carry x-rift-binary-error"
    );
    let _ = manager.delete_imposter(19953).await;
}

// AC4 (regression guard): with the flag absent, #323/#269 lenient behavior is unchanged —
// a failing decorate still serves 200 + fallback body + the signal header.
#[tokio::test]
async fn default_lenient_decorate_still_200() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19954, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "decorate": "function (request, response) { throw new Error('boom'); }" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19954/x")
        .send()
        .await
        .expect("send");
    let status = resp.status();
    let has_header = resp.headers().contains_key("x-rift-decorate-error");
    let body = resp.text().await.expect("body");
    assert_eq!(status, 200, "default mode stays lenient (issue #269)");
    assert!(
        has_header,
        "default mode still signals the failure (issue #323)"
    );
    assert_eq!(
        body, "original",
        "default mode still serves the fallback body"
    );
    let _ = manager.delete_imposter(19954).await;
}
