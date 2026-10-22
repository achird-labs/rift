//! Issue #1583: Mountebank's `copy` reads a missing request field as `undefined`, which a regex
//! matches as text (`RegExp.exec(undefined)`), and `globalObjectReplace` replaces tokens in every
//! key of the response, header names included. Rift left the token for a missing field, and a token
//! in a header name reached the builder and answered `500 Response build error`.

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

/// Creates the imposter on an auto-assigned port (issue #1438) and returns it.
async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config decodes");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send")
}

fn imposter(is: Value, from: Value, selector: &str) -> Value {
    json!({ "protocol": "http", "stubs": [ { "responses": [ {
        "is": is,
        "behaviors": [ { "copy": {
            "from": from,
            "into": "${v}",
            "using": { "method": "regex", "selector": selector }
        } } ]
    } ] } ] })
}

#[tokio::test]
async fn a_missing_query_field_is_copied_as_undefined() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        imposter(
            json!({ "body": "v=${v}" }),
            json!({ "query": "nosuch" }),
            ".*",
        ),
    )
    .await;
    let resp = get(port, "/p").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "v=undefined");
}

#[tokio::test]
async fn a_token_in_a_header_name_is_served_replaced() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        imposter(
            json!({ "headers": { "X-${v}": "${v}" }, "body": "ok" }),
            json!("path"),
            "\\w+",
        ),
    )
    .await;
    let resp = get(port, "/p").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-p").and_then(|v| v.to_str().ok()),
        Some("p")
    );
    assert_eq!(resp.text().await.expect("body"), "ok");
}

/// A token with no value stays in the name, which is not a valid header name: still a 500, as
/// in Mountebank (`ERR_INVALID_HTTP_TOKEN`), but one that names the header.
#[tokio::test]
async fn a_header_name_left_invalid_answers_500_naming_it() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        imposter(
            json!({ "headers": { "X-${v}": "1" }, "body": "ok" }),
            json!("path"),
            "\\d+",
        ),
    )
    .await;
    let resp = get(port, "/p").await;
    assert_eq!(resp.status(), 500);
    let body: Value = resp.json().await.expect("json body");
    let message = body["errors"][0]["message"].as_str().unwrap_or_default();
    assert_eq!(
        message, "Response build error: invalid header name \"X-${v}\"",
        "{body}"
    );
}
