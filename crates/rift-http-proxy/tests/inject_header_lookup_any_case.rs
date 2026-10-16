//! Issue #1368: an inject script finds a request header under any spelling. Mountebank hands
//! scripts the names the client sent; Rift's listener Title-Cases them and cannot see the wire
//! case, so lookups are case-insensitive instead.

use std::sync::Arc;

use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;

#[tokio::test]
async fn an_inject_reads_a_header_in_lowercase_whatever_the_client_sent() {
    let manager = Arc::new(ImposterManager::new());
    // Created through the manager: the admin API refuses `inject` without `--allowInjection`.
    let config = serde_json::from_value(json!({"protocol": "http", "stubs": [
        {"responses": [{"inject":
            "function (config) { return { statusCode: 200, body: config.request.headers['x-client-id'] + '|' + Object.keys(config.request.headers).indexOf('X-Client-Id') }; }"
        }]}
    ]}))
    .expect("imposter config");
    let port = manager.create_imposter(config).await.expect("create");

    let body = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/h"))
        .header("X-Client-ID", "abc")
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    let (value, index) = body.split_once('|').expect("two parts");
    assert_eq!(value, "abc");
    assert_ne!(
        index, "-1",
        "enumeration still shows the Title-Cased name: {body}"
    );

    let _ = manager.delete_imposter(port).await;
}
