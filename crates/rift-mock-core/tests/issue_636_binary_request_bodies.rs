//! Issue #636: a binary request body (protobuf, gzip, an image upload) used to go through
//! `String::from_utf8_lossy`, silently replacing the offending bytes with U+FFFD. The recorded
//! request, predicate matches, and script/inject bodies then no longer reflected what the client
//! actually sent — irreversibly, with no error.
//!
//! These tests drive the real imposter serve loop end-to-end (`ImposterManager` + reqwest, same
//! pattern as `handler_error_responses.rs`) and assert:
//!   - a binary body round-trips through the recorded request's base64 `body` + `_mode: "binary"`
//!   - a `_rift.script` (Rhai) request sees the same base64 body plus an `isBinary` flag
//!   - a text body is completely unaffected (no `_mode`, plain string body)

#![cfg(feature = "javascript")]

use rift_mock_core::imposter::{ImposterManager, ResponseMode};
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: serde_json::Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    // Give the listener a moment to bind before the request (matches the sibling HTTP tests).
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

// A binary body must be recorded losslessly: `body` base64-decodes back to the exact bytes the
// client sent, and `mode` is `Binary` (serialized as `_mode: "binary"`).
#[tokio::test]
async fn binary_request_body_round_trips_through_recorded_request() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }),
    )
    .await;

    let original: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x02, 0xC0, 0xC1];
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/x"))
        .body(original.to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1, "exactly one request was recorded");
    let req = &recorded[0];

    assert_eq!(
        req.mode,
        ResponseMode::Binary,
        "an invalid-UTF-8 body must be classified as binary"
    );
    let body = req.body.as_deref().expect("body present");
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("recorded body must be valid base64");
    assert_eq!(
        decoded, original,
        "the recorded body must decode back to the exact original bytes"
    );

    // Pin the wire shape too: `_mode` must be present and `"binary"` on the JSON the admin API
    // would actually serve.
    let value = serde_json::to_value(req).expect("serializes");
    assert_eq!(value["_mode"], "binary");

    manager.delete_all().await;
}

// A text body must be completely unaffected by the #636 change: no `_mode` field at all, and the
// recorded `body` is the exact text (not base64).
#[tokio::test]
async fn text_request_body_recorded_unchanged_with_no_mode_field() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/x"))
        .body("hello world")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1);
    let req = &recorded[0];

    assert_eq!(req.mode, ResponseMode::Text);
    assert_eq!(req.body.as_deref(), Some("hello world"));

    let value = serde_json::to_value(req).expect("serializes");
    assert!(
        value.get("_mode").is_none(),
        "a text body must not carry `_mode` on the wire: {value}"
    );

    manager.delete_all().await;
}

// A `_rift.script` (Rhai) handler must see the base64 body plus `isBinary: true` for a binary
// request — never the pre-#636 U+FFFD-mangled text, and never silently indistinguishable from a
// real text body.
#[tokio::test]
async fn rift_script_request_exposes_binary_body_and_flag() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "rhai", "code":
                        "fn respond(ctx) { http(200, #{isBinary: ctx.request.isBinary, body: ctx.request.body}) }"
                    } }
                }]
            }]
        }),
    )
    .await;

    let original: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x02, 0xC0, 0xC1];
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/x"))
        .body(original.to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["isBinary"], true,
        "the script must see isBinary: true for a binary body, got: {body}"
    );
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body["body"].as_str().expect("body is a string"))
        .expect("script-visible body must be valid base64");
    assert_eq!(decoded, original);

    manager.delete_all().await;
}

// Sanity check the flag flips back to `false` for an ordinary text body — the `isBinary` field
// must actually distinguish the two, not just always report `true`/`false`.
#[tokio::test]
async fn rift_script_request_text_body_is_not_binary() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "rhai", "code":
                        "fn respond(ctx) { http(200, #{isBinary: ctx.request.isBinary, body: ctx.request.body}) }"
                    } }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/x"))
        .body("plain text")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["isBinary"], false);
    assert_eq!(body["body"], "plain text");

    manager.delete_all().await;
}
