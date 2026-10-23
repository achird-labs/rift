//! Issue #1672: a `repeat` beside a behavior in a `behaviors` array element is a custom key, as in
//! Mountebank 2.9.5, where `behaviorsValidator.js` admits it unchecked and only `response.repeat`
//! drives the response cycle. Rift used to lift it to the response's `repeat`, so the response was
//! served `n` times per cycle instead of once.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

#[tokio::test]
async fn an_element_repeat_beside_a_behavior_does_not_repeat_the_response() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");

    let reply = client
        .post(format!("{base}/imposters"))
        .json(&json!({"protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "A"}, "behaviors": [{"wait": 1, "repeat": 3}]},
            {"is": {"body": "B"}}
        ]}]}))
        .send()
        .await
        .expect("POST /imposters");
    let status = reply.status().as_u16();
    let created: Value = reply.json().await.expect("JSON body");
    assert_eq!(status, 201, "{created}");
    let port = created["port"].as_u64().expect("assigned port");

    let mut served = Vec::new();
    for _ in 0..4 {
        let body = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("GET")
            .text()
            .await
            .expect("body");
        served.push(body);
    }
    assert_eq!(served, ["A", "B", "A", "B"]);

    // Written back as Mountebank writes it: inside its element, with no response `repeat`.
    let imposter: Value = client
        .get(format!("{base}/imposters/{port}?replayable=true"))
        .send()
        .await
        .expect("GET /imposters/:port")
        .json()
        .await
        .expect("JSON body");
    let response = &imposter["stubs"][0]["responses"][0];
    assert_eq!(response.get("repeat"), None, "{response}");
    assert_eq!(response["behaviors"], json!([{"wait": 1, "repeat": 3}]));

    let _ = manager
        .delete_imposter(u16::try_from(port).expect("port"))
        .await;
}
