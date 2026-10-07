//! `proxyTransparent` forwards every request and records nothing — also when the proxy declares
//! `predicateGenerators` (or `addWaitBehavior` / `addDecorateBehavior`), which Rift otherwise reads
//! as "record a stub". Found by the v0.20.0 docs audit: the stub-recording branch never looked at
//! the mode, so a transparent proxy with generators recorded a stub and replayed it, and the
//! upstream saw one request out of three.

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn start() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    (
        reqwest::Client::new(),
        format!("http://{}", running.local_addr()),
        manager,
    )
}

async fn create(client: &reqwest::Client, admin: &str, imposter: Value) {
    let response = client
        .post(format!("{admin}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST");
    assert_eq!(response.status().as_u16(), 201, "{imposter}");
}

async fn imposter(client: &reqwest::Client, admin: &str, port: u16) -> Value {
    client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json")
}

#[tokio::test]
async fn a_transparent_proxy_with_generators_forwards_every_request_and_records_nothing() {
    let (client, admin, manager) = start().await;
    let upstream = free_port();
    create(
        &client,
        &admin,
        json!({"port": upstream, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "up"}}]}]}),
    )
    .await;
    let proxy = free_port();
    create(
        &client,
        &admin,
        json!({"port": proxy, "protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyTransparent",
            "predicateGenerators": [{"matches": {"path": true}}]
        }}]}]}),
    )
    .await;

    for _ in 0..3 {
        let body = client
            .get(format!("http://127.0.0.1:{proxy}/t"))
            .send()
            .await
            .expect("proxied")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "up");
    }

    let hits = imposter(&client, &admin, upstream).await["requests"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(hits, 3, "every request reaches the upstream");
    let stubs = imposter(&client, &admin, proxy).await["stubs"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(stubs, 1, "nothing recorded beside the proxy stub");
    manager.delete_all().await;
}
