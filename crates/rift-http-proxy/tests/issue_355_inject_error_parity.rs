//! Issue #355 Item 5 (AC5): a Mountebank `inject` response function that THROWS must yield a
//! Mountebank-shaped 400 error — `{"errors":[{"code":"invalid injection","message":...}]}` — not
//! a bare 500. The script failing to produce a valid response is a config problem (client error),
//! matching Mountebank's error parity.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

// A throwing inject response returns HTTP 400 with the Mountebank error body shape, and keeps the
// x-rift-imposter / x-rift-inject-error headers.
#[tokio::test]
async fn throwing_inject_returns_mountebank_400() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 22650, "protocol": "http", "stubs": [
            { "responses": [{ "inject": "function (config) { throw new Error('boom-inject'); }" }] }
        ]
    }))
    .expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:22650/x")
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        400,
        "a throwing inject must return 400 (Mountebank error parity), not 500"
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
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom-inject")),
        "the error message must surface the script failure, got: {body}"
    );

    let _ = manager.delete_imposter(22650).await;
}
