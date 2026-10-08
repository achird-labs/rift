//! Issue #1326: a `copy`/`lookup` XPath `using` carries Mountebank's `ns` prefix→URI map. Rift
//! accepted the key and dropped it, so a prefixed selector matched nothing on a namespaced
//! document and the token was replaced with the empty string.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const NAMESPACED: &str =
    r#"<mb:request xmlns:mb="http://example.com/mb"><mb:name>alice</mb:name></mb:request>"#;

async fn admin() -> (rift_http_proxy::admin_api::RunningAdminApi, String) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

fn copy_stub(using: serde_json::Value) -> serde_json::Value {
    json!({ "responses": [{
        "is": { "statusCode": 200, "body": "hello ${NAME}" },
        "_behaviors": { "copy": { "from": "body", "into": "${NAME}", "using": using } }
    }] })
}

/// Creates an imposter with `stub` on an ephemeral port and returns the body served for
/// `NAMESPACED`.
async fn served_body(stub: serde_json::Value) -> String {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        created.status(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let body = client
        .post(format!("http://127.0.0.1:{port}/"))
        .body(NAMESPACED)
        .send()
        .await
        .expect("request to the imposter")
        .text()
        .await
        .expect("body");
    running.shutdown().await;
    body
}

#[tokio::test]
async fn copy_xpath_with_ns_copies_the_prefixed_value() {
    let body = served_body(copy_stub(json!({
        "method": "xpath",
        "selector": "//mb:name",
        "ns": { "mb": "http://example.com/mb" }
    })))
    .await;
    assert_eq!(body, "hello alice");
}

#[tokio::test]
async fn copy_xpath_without_ns_selects_nothing_prefixed() {
    let body = served_body(copy_stub(
        json!({ "method": "xpath", "selector": "//mb:name" }),
    ))
    .await;
    assert_eq!(body, "hello ");
}

#[tokio::test]
async fn the_ns_map_survives_the_replayable_listing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let using = json!({ "method": "xpath", "selector": "//mb:name", "ns": { "mb": "http://example.com/mb" } });
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [copy_stub(using.clone())] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let imposter: serde_json::Value = client
        .get(format!("{base}/imposters/{port}?replayable=true"))
        .send()
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("json");
    assert_eq!(
        imposter["stubs"][0]["responses"][0]["behaviors"][0]["copy"]["using"], using,
        "{imposter}"
    );
    running.shutdown().await;
}
