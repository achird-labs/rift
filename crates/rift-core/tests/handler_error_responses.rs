//! Issue #500: end-to-end handler coverage for the three script-failure response contracts.
//!
//! The equivalent parity tests already exist as integration binaries under
//! `crates/rift-http-proxy/tests/` (issue_355/440/323/375), but CI's main job runs
//! `cargo test --workspace --all-features --lib`, and `--lib` excludes every `tests/*.rs` binary —
//! so those contracts had no CI-run coverage (tracked as #510). These tests drive the full handler
//! through a real listener (`ImposterManager` + reqwest) and assert status/headers/body shape per
//! contract, and CI runs them via a dedicated `--test handler_error_responses` step.
//!
//! They live in their own integration binary — a SEPARATE PROCESS with its own MB JS script pool —
//! rather than in the rift-core lib test binary, so they don't contend for the shared script pool
//! with the timing-sensitive bounded-matcher unit tests (whose runaway-script cases park pool
//! workers; co-locating server tests there starves the 60s inject-matching deadline under CI load).

#![cfg(feature = "javascript")]

use rift_core::imposter::ImposterManager;
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    // Give the listener a moment to bind before the request (matches the sibling HTTP tests).
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn get(port: u16) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send")
}

// Contract 1: a throwing response `inject` → 400 Mountebank error body + inject-error header.
#[tokio::test]
async fn throwing_inject_response_returns_400() {
    let manager = ImposterManager::new();
    create(
        &manager,
        serde_json::json!({
            "port": 19750, "protocol": "http", "stubs": [
                { "responses": [{ "inject": "function (config) { throw new Error('boom-inject'); }" }] }
            ]
        }),
    )
    .await;

    let resp = get(19750).await;
    assert_eq!(resp.status(), 400, "a throwing inject is a 400, not a 500");
    assert!(resp.headers().contains_key("x-rift-imposter"));
    assert!(
        resp.headers().contains_key("x-rift-inject-error"),
        "the inject-error marker header must be present"
    );
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["errors"][0]["code"], "invalid injection");
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom-inject")),
        "error message must surface the script failure, got: {body}"
    );

    let _ = manager.delete_imposter(19750).await;
}

// Contract 2 (lenient): a throwing `decorate` → the original response is still served (status +
// body unchanged) with an `x-rift-decorate-error` signal header, not a 500.
#[tokio::test]
async fn decorate_error_lenient_serves_undecorated_with_header() {
    let manager = ImposterManager::new();
    create(
        &manager,
        serde_json::json!({
            "port": 19751, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "decorate": "function (request, response) { throw new Error('boom-decorate'); }" } }] }
            ]
        }),
    )
    .await;

    let resp = get(19751).await;
    let status = resp.status();
    let has_header = resp.headers().contains_key("x-rift-decorate-error");
    let body = resp.text().await.expect("body");
    assert_eq!(
        status, 200,
        "lenient decorate failure still serves the original response"
    );
    assert!(
        has_header,
        "the undecorated response must carry x-rift-decorate-error"
    );
    assert_eq!(
        body, "original",
        "the original (undecorated) body is served"
    );

    let _ = manager.delete_imposter(19751).await;
}

// Contract 2 (strict): the same failing `decorate` under `strictBehaviors` → 500, the fallback
// body is NOT served, and the signal header is still present.
#[tokio::test]
async fn decorate_error_strict_returns_500() {
    let manager = ImposterManager::new();
    create(
        &manager,
        serde_json::json!({
            "port": 19752, "protocol": "http", "strictBehaviors": true, "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "decorate": "function (request, response) { throw new Error('boom-decorate'); }" } }] }
            ]
        }),
    )
    .await;

    let resp = get(19752).await;
    let status = resp.status();
    let has_header = resp.headers().contains_key("x-rift-decorate-error");
    let body = resp.text().await.expect("body");
    assert_eq!(status, 500, "strict decorate failure fails loud with 500");
    assert!(has_header, "the 500 must still carry x-rift-decorate-error");
    assert_ne!(
        body, "original",
        "the fallback body must NOT be served in strict mode"
    );
    assert!(
        body.contains("decorate failed"),
        "the 500 body must name the strict decorate failure, got: {body}"
    );

    let _ = manager.delete_imposter(19752).await;
}

// Contract 3: a throwing predicate `inject` (matcher error) → 400 with the DISTINCT
// "invalid predicate injection" code (vs "invalid injection" for a response inject), and the
// inject-error header. Any other matcher error keeps the 5xx backend mapping (unit-tested in
// handler.rs `matcher_error_response_tests`, since a listener test can't easily provoke it).
#[tokio::test]
async fn predicate_inject_error_returns_400() {
    let manager = ImposterManager::new();
    create(
        &manager,
        serde_json::json!({
            "port": 19753, "protocol": "http", "stubs": [
                { "predicates": [{ "inject": "function (config) { throw new Error('boom-predicate'); }" }],
                  "responses": [{ "is": { "statusCode": 200, "body": "unreached" } }] }
            ]
        }),
    )
    .await;

    let resp = get(19753).await;
    assert_eq!(resp.status(), 400, "a throwing predicate inject is a 400");
    assert!(resp.headers().contains_key("x-rift-inject-error"));
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["errors"][0]["code"], "invalid predicate injection",
        "matcher inject error uses the DISTINCT predicate-injection code, got: {body}"
    );
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom-predicate")),
        "error message must surface the predicate script failure, got: {body}"
    );

    let _ = manager.delete_imposter(19753).await;
}
