//! Issue #323 gate: a response behavior that the stub author requested but that FAILS must not be
//! a completely silent success. The fallback body is still served (preserving #269's lenient
//! shellTransform contract), but a visible `x-rift-<behavior>-error` header is attached so the
//! client can tell the behavior was skipped.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

// AC1: a decorate script that throws attaches x-rift-decorate-error (body unchanged).
#[tokio::test]
async fn decorate_failure_sets_error_header() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19891, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "decorate": "function (request, response) { throw new Error('boom'); }" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19891/x")
        .send()
        .await
        .expect("send");
    let has_header = resp.headers().contains_key("x-rift-decorate-error");
    let body = resp.text().await.expect("body");
    assert!(
        has_header,
        "a failing decorate must attach x-rift-decorate-error, not be a silent success"
    );
    assert_eq!(body, "original", "the fallback body is still served");
    let _ = manager.delete_imposter(19891).await;
}

// AC2: a failing shellTransform attaches x-rift-shelltransform-error (body kept, per #269).
#[tokio::test]
async fn shell_transform_failure_sets_error_header() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19892, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "shellTransform": "exit 1" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19892/x")
        .send()
        .await
        .expect("send");
    let has_header = resp.headers().contains_key("x-rift-shelltransform-error");
    let body = resp.text().await.expect("body");
    assert!(
        has_header,
        "a failing shellTransform must attach x-rift-shelltransform-error"
    );
    assert_eq!(body, "original", "the body is kept unchanged (issue #269)");
    let _ = manager.delete_imposter(19892).await;
}

// AC3: a binary-mode body that isn't valid base64 attaches x-rift-binary-error.
#[tokio::test]
async fn binary_decode_failure_sets_error_header() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19893, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" } }] }
            ]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19893/x")
        .send()
        .await
        .expect("send");
    assert!(
        resp.headers().contains_key("x-rift-binary-error"),
        "a failing binary base64 decode must attach x-rift-binary-error"
    );
    let _ = manager.delete_imposter(19893).await;
}
