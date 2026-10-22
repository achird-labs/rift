//! Issue #355 Item 5 (AC5): a Mountebank `inject` response function that THROWS must yield a
//! Mountebank-shaped 500 error (issue #1581: #355 had assumed 400) — `{"errors":[{"code":"invalid injection","message":...}]}` — with
//! Mountebank's `data` (the thrown error's message).

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

// A throwing inject response returns HTTP 500 with the Mountebank error body shape, and keeps the
// x-rift-imposter / x-rift-inject-error headers.
#[tokio::test]
async fn throwing_inject_returns_mountebank_500() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http", "stubs": [
            { "responses": [{ "inject": "function (config) { throw new Error('boom-inject'); }" }] }
        ]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        500,
        "a throwing inject must return 500 (Mountebank error parity)"
    );
    assert!(
        resp.headers().contains_key("x-rift-imposter"),
        "the imposter marker header must be preserved"
    );
    assert!(
        resp.headers().contains_key("x-rift-inject-error"),
        "the inject-error marker header must be present"
    );

    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["errors"][0]["code"], "invalid injection",
        "error code must be Mountebank's 'invalid injection', got: {body}"
    );
    assert_eq!(
        body["errors"][0]["data"], "boom-inject",
        "data is the thrown error's message (issue #1581), got: {body}"
    );
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom-inject")),
        "the error message must surface the script failure, got: {body}"
    );

    let _ = manager.delete_imposter(port).await;
}
