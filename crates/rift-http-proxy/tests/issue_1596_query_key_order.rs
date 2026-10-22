//! Issue #1596: `request.query` and `request.form` list their keys in the order the client sent
//! them, as Mountebank's `querystring.parse` does. A script that `JSON.stringify`s either one used
//! to see an order that changed from request to request (the map's hasher is seeded per map).

use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::time::Duration;

/// An imposter whose inject answers `Q <query> F <form>`. The non-JSON prefix keeps the body a
/// plain string, so a test sees the key order byte for byte.
async fn echo_imposter(manager: &ImposterManager) -> u16 {
    let config = serde_json::from_value(json!({"protocol": "http", "stubs": [
        {"responses": [{"inject":
            "function (config) { return { statusCode: 200, body: 'Q ' + JSON.stringify(config.request.query) + ' F ' + JSON.stringify(config.request.form || null) }; }"
        }]}
    ]}))
    .expect("imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

#[tokio::test]
async fn query_keys_keep_request_order_on_every_request() {
    let manager = ImposterManager::new();
    let port = echo_imposter(&manager).await;
    let client = reqwest::Client::new();
    // Many requests: the order used to differ between them, so one match proves little.
    for i in 0..50 {
        let body = client
            .get(format!("http://127.0.0.1:{port}/x?b=1&a=2&c=3&d=4&e=5"))
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body");
        assert_eq!(
            body, r#"Q {"b":"1","a":"2","c":"3","d":"4","e":"5"} F null"#,
            "request {i}"
        );
    }
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn integer_like_query_keys_come_first_as_in_a_js_object() {
    let manager = ImposterManager::new();
    let port = echo_imposter(&manager).await;
    let body = reqwest::get(format!("http://127.0.0.1:{port}/x?b=1&10=x&2=y&a=3"))
        .await
        .expect("send")
        .text()
        .await
        .expect("body");
    assert_eq!(body, r#"Q {"2":"y","10":"x","b":"1","a":"3"} F null"#);
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn form_keys_keep_request_order() {
    let manager = ImposterManager::new();
    let port = echo_imposter(&manager).await;
    let client = reqwest::Client::new();
    for i in 0..50 {
        let body = client
            .post(format!("http://127.0.0.1:{port}/x"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("z=1&y=2&x=3&w=4&v=5")
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body");
        assert_eq!(
            body, r#"Q {} F {"z":"1","y":"2","x":"3","w":"4","v":"5"}"#,
            "request {i}"
        );
    }
    let _ = manager.delete_imposter(port).await;
}
