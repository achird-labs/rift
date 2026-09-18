//! Issue #1151: `defaultResponse` with `_mode: "binary"` and a body that is not valid base64 was
//! served as the raw text of the invalid base64, behind a `warn!` — a `200` carrying bytes nobody
//! asked for, with the only signal server-side.
//!
//! The fix is **parity with the `is` path**, not a config-door refusal. A stub `is` response with
//! an undecodable binary body has been admitted at every door and handled at serve time since
//! #323/#375: lenient mode serves the raw body *with* `x-rift-binary-error: true`, and
//! `strictBehaviors` answers `500`. `defaultResponse` was the one decode site that had neither.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn get(port: u16) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/no-stub-matches-this"))
        .send()
        .await
        .expect("send")
}

// The defect itself: lenient mode must still serve the configured response, but no longer
// silently — the raw fallback now carries the same header the `is` path attaches.
#[tokio::test]
async fn an_undecodable_binary_default_response_is_flagged_not_silent() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21910, "protocol": "http", "stubs": [],
            "defaultResponse": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" }
        }),
    )
    .await;

    let resp = get(21910).await;
    assert_eq!(
        resp.status(),
        200,
        "lenient mode keeps the configured status"
    );
    assert_eq!(
        resp.headers()
            .get("x-rift-binary-error")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "a failed binary decode on defaultResponse must attach x-rift-binary-error, as the is path does"
    );
    assert_eq!(
        resp.headers()
            .get("x-rift-default-response")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "it is still the default response"
    );
    assert_eq!(
        resp.text().await.expect("body"),
        "not!valid!base64!",
        "lenient mode serves the raw body, exactly as the is path does (#269/#323)"
    );
    let _ = manager.delete_imposter(21910).await;
}

// `strictBehaviors` turns the same failure into a 500 — the #375 contract the default site never
// picked up, because the strict flag was scoped to the matched-stub block.
#[tokio::test]
async fn strict_behaviors_makes_an_undecodable_binary_default_response_a_500() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21911, "protocol": "http", "stubs": [], "strictBehaviors": true,
            "defaultResponse": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" }
        }),
    )
    .await;

    let resp = get(21911).await;
    assert_eq!(
        resp.status(),
        500,
        "strictBehaviors must fail loud on the default response too"
    );
    assert!(resp.headers().contains_key("x-rift-binary-error"));
    let _ = manager.delete_imposter(21911).await;
}

// A non-string body is serialized to JSON text before the decode, so under `_mode: "binary"` it
// *always* fails to decode — and was served as that JSON text with no signal at all.
#[tokio::test]
async fn an_object_body_in_binary_mode_is_flagged() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21912, "protocol": "http", "stubs": [],
            "defaultResponse": { "statusCode": 200, "body": { "a": 1 }, "_mode": "binary" }
        }),
    )
    .await;

    let resp = get(21912).await;
    assert!(
        resp.headers().contains_key("x-rift-binary-error"),
        "an object body can never be valid base64, so binary mode must flag it"
    );
    let _ = manager.delete_imposter(21912).await;
}

// The happy path had no test anywhere: a valid base64 default body serves the decoded bytes and
// carries no error header.
#[tokio::test]
async fn a_valid_binary_default_response_serves_the_decoded_bytes() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21913, "protocol": "http", "stubs": [],
            // base64("hello")
            "defaultResponse": { "statusCode": 200, "body": "aGVsbG8=", "_mode": "binary" }
        }),
    )
    .await;

    let resp = get(21913).await;
    assert_eq!(resp.status(), 200);
    assert!(
        !resp.headers().contains_key("x-rift-binary-error"),
        "a clean decode carries no error header"
    );
    assert_eq!(resp.bytes().await.expect("body").as_ref(), b"hello");
    let _ = manager.delete_imposter(21913).await;
}
