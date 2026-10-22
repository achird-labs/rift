//! Issue #797: every engine-originated error envelope carries a `type` slug, and `code` is frozen.
//!
//! This is a **door walk**: it drives each error door the issue inventoried and asserts two things
//! per door — the new `type` is the slug the design pins, *and* `code` is byte-identical to what
//! 0.14.0 served. The second half is the non-breakage proof. It is deliberately asserted here
//! rather than argued in a PR description, because `rift-conformance` pins these same `code` values
//! and a regression would otherwise only surface in that downstream repo.

use rift_http_proxy::imposter::ImposterManager;
use serde_json::Value;
use std::time::Duration;

/// Assert one door: `code` unchanged, `type` present and equal to the pinned slug.
fn assert_door(body: &Value, expected_code: &str, expected_type: &str, door: &str) {
    assert_eq!(
        body["errors"][0]["code"], expected_code,
        "{door}: `code` must stay byte-identical to 0.14.0 (frozen), got: {body}"
    );
    assert_eq!(
        body["errors"][0]["type"], expected_type,
        "{door}: `type` must be the pinned slug, got: {body}"
    );
}

async fn get(url: &str) -> (u16, Value) {
    let resp = reqwest::Client::new().get(url).send().await.expect("send");
    let status = resp.status().as_u16();
    let body = resp.json::<Value>().await.expect("json body");
    (status, body)
}

// A disabled imposter answers 503. `code` stays "503"; `type` names the door.
#[tokio::test]
async fn disabled_imposter_door() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    manager
        .get_imposter(port)
        .expect("imposter exists")
        .set_enabled(false);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 503);
    assert_door(&body, "503", "imposter disabled", "disabled imposter");
}

// A throwing response-`inject` answers 500 with Mountebank's slug already in `code` (#355). On the
// five doors whose `code` is already a slug, `type` must equal it — invariant 3.
#[tokio::test]
async fn response_inject_error_door_has_type_equal_to_code() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "stubs": [
            { "responses": [{ "inject": "function (config) { throw new Error('boom'); }" }] }
        ]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 500);
    assert_door(
        &body,
        "invalid injection",
        "invalid injection",
        "response inject error",
    );
}

// A throwing predicate-`inject` answers 500 with the predicate-specific slug.
#[tokio::test]
async fn predicate_inject_error_door_has_type_equal_to_code() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "stubs": [{
            "predicates": [{ "inject": "function (config) { throw new Error('boom'); }" }],
            "responses": [{ "is": { "statusCode": 200 } }]
        }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 500);
    assert_door(
        &body,
        "invalid predicate injection",
        "invalid predicate injection",
        "predicate inject error",
    );
}

// A `_rift.script` that throws answers 500. `code` stays "500" — the status string — while `type`
// distinguishes it from every other 500, which is the whole point of the field.
#[tokio::test]
async fn script_error_door() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "stubs": [{
            "responses": [{
                "_rift": { "script": { "engine": "rhai", "code": "throw \"script-boom\"" } }
            }]
        }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 500);
    assert_door(&body, "500", "script error", "script error");
}

// An over-size request body answers 413 purely from the default status→slug table — no door-side
// change was needed for it, which is the property that keeps ~85 call sites untouched.
#[tokio::test]
async fn oversize_body_door_uses_the_default_table() {
    // A small configured ceiling (issue #1439): the default is 512 MiB.
    let manager = ImposterManager::new().with_body_limits(
        rift_http_proxy::imposter::body_limits::BodyLimits::new(1024 * 1024, None)
            .expect("valid limits"),
    );
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // One byte past the ceiling, with a known length: the refusal comes before the body is read.
    // That alone used to *cause* a reset, not prevent one: closing on the unread body made the
    // kernel send a RST, and about 40% of h1 clients saw it instead of the 413. The lingering
    // close (#1460) drains the body before closing, which is what delivers the 413
    // (`issue_1460_lingering_close.rs` pins it deterministically).
    let huge = "x".repeat(1024 * 1024 + 1);
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/x"))
        .body(huge)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 413);
    let body: Value = resp.json().await.expect("json body");
    assert_door(&body, "413", "request too large", "oversize body");
}

// strictBehaviors decorate failure answers 500 with the shared behavior slug.
#[tokio::test]
async fn strict_behaviors_decorate_door() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "strictBehaviors": true,
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "original" },
                "_behaviors": { "decorate": "function (request, response) { throw new Error('dec-boom'); }" }
            }]
        }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 500);
    assert_door(&body, "500", "behavior error", "strictBehaviors decorate");
}

// The second behavior door. `decorate` and `shellTransform` are separate call sites that happen to
// share `ErrorKind::BehaviorError` — walking only one would let the other drift to a different slug
// unnoticed, since nothing in the type system ties them together.
#[tokio::test]
async fn strict_behaviors_shell_transform_door() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "strictBehaviors": true,
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "original" },
                "_behaviors": { "shellTransform": "exit 1" }
            }]
        }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, 500);
    assert_door(
        &body,
        "500",
        "behavior error",
        "strictBehaviors shellTransform",
    );
}

// `defaultForward` to a dead upstream answers 502 straight from the default status→slug table —
// the second door (with over-size body) proving the table carries doors nobody had to touch.
#[tokio::test]
async fn default_forward_upstream_failure_door() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        // Port 1 is reserved and never listening, so the forward fails to connect.
        "defaultForward": "http://127.0.0.1:1",
        "stubs": []
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = get(&format!("http://127.0.0.1:{port}/anything")).await;
    assert_eq!(status, 502);
    assert_door(&body, "502", "upstream failure", "defaultForward upstream");
}
