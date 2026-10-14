//! Issue #1399: `DELETE /imposters/{port}` answers the imposter **as it was**, the document
//! `GET /imposters/{port}` prints, with the request counters it ended on. It used to hand-build a
//! body that printed `name: null`, dropped `defaultResponse`, and reported `numberOfRequests: 0`.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

async fn start_server() -> (String, rift_http_proxy::admin_api::RunningAdminApi) {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let running = AdminApiServer::new(addr, manager, None)
        .bind()
        .await
        .expect("bind admin");
    (format!("http://{}", running.local_addr()), running)
}

async fn create(admin: &str, client: &reqwest::Client, config: Value) -> u16 {
    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&config)
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.expect("JSON body");
    body["port"].as_u64().expect("assigned port") as u16
}

async fn admin_json(req: reqwest::RequestBuilder) -> Value {
    let resp = req.send().await.expect("send");
    assert_eq!(resp.status().as_u16(), 200);
    resp.json().await.expect("JSON body")
}

async fn hit(client: &reqwest::Client, port: u16, path: &str) {
    client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("imposter request");
}

#[tokio::test]
async fn delete_answers_the_final_state_as_get_prints_it() {
    let (admin, running) = start_server().await;
    let client = reqwest::Client::new();
    let port = create(
        &admin,
        &client,
        json!({
            "port": 0, "protocol": "http",
            "defaultResponse": { "statusCode": 404, "body": "not found" },
            "stubs": [{
                "predicates": [{ "equals": { "path": "/a" } }],
                "responses": [{ "is": { "body": "A" } }]
            }]
        }),
    )
    .await;
    hit(&client, port, "/a").await;
    hit(&client, port, "/missing").await;

    let got = admin_json(client.get(format!("{admin}/imposters/{port}"))).await;
    let deleted = admin_json(client.delete(format!("{admin}/imposters/{port}"))).await;

    assert!(
        deleted.get("name").is_none(),
        "unnamed prints no name: {deleted}"
    );
    assert_eq!(deleted["numberOfRequests"], 2, "{deleted}");
    assert_eq!(deleted["defaultResponse"]["statusCode"], 404, "{deleted}");
    assert_eq!(deleted["defaultResponse"]["body"], "not found", "{deleted}");

    let strip = |mut v: Value| {
        let o = v.as_object_mut().expect("object");
        o.remove("numberOfRequests");
        o.remove("requests");
        v
    };
    assert_eq!(strip(deleted), strip(got));
    running.shutdown().await;
}

#[tokio::test]
async fn delete_lists_the_recorded_requests() {
    let (admin, running) = start_server().await;
    let client = reqwest::Client::new();
    let port = create(
        &admin,
        &client,
        json!({ "port": 0, "protocol": "http", "recordRequests": true, "stubs": [] }),
    )
    .await;
    hit(&client, port, "/first").await;
    hit(&client, port, "/second").await;

    let deleted = admin_json(client.delete(format!("{admin}/imposters/{port}"))).await;
    assert_eq!(deleted["numberOfRequests"], 2, "{deleted}");
    let paths: Vec<&str> = deleted["requests"]
        .as_array()
        .unwrap_or_else(|| panic!("requests array in {deleted}"))
        .iter()
        .map(|r| r["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, ["/first", "/second"]);
    running.shutdown().await;
}
