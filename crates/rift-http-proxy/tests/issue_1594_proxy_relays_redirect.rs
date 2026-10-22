//! Issue #1594: a `proxy` stub relays an upstream 30x and records it, as Mountebank does (node's
//! `http.request` never follows redirects). The proxy client used to follow up to ten hops, so the
//! caller saw the redirect target's answer and the recording held that answer under the origin's
//! predicates.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn start() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    // The test's own client must not follow the 302 either, or it would mask the proxy's answer.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    (client, format!("http://{}", running.local_addr()), manager)
}

async fn create(client: &reqwest::Client, admin: &str, imposter: Value) -> u16 {
    let response = client
        .post(format!("{admin}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST");
    assert_eq!(response.status().as_u16(), 201, "{imposter}");
    let body: Value = response.json().await.expect("json");
    body["port"].as_u64().expect("assigned port") as u16
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
async fn a_proxy_relays_and_records_an_upstream_302_without_following_it() {
    let (client, admin, manager) = start().await;
    let target = create(
        &client,
        &admin,
        json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "landed"}}]}]}),
    )
    .await;
    let location = format!("http://127.0.0.1:{target}/landed");
    let origin = create(
        &client,
        &admin,
        json!({"protocol": "http", "stubs": [{"responses": [{"is": {
            "statusCode": 302, "headers": {"Location": location}, "body": "moved"}}]}]}),
    )
    .await;
    let proxy = create(
        &client,
        &admin,
        json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{origin}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{"matches": {"path": true}}]
        }}]}]}),
    )
    .await;

    // First call proxies, second replays the recording: both must be the redirect.
    for round in 0..2 {
        let response = client
            .get(format!("http://127.0.0.1:{proxy}/start"))
            .send()
            .await
            .expect("proxied request");
        assert_eq!(response.status().as_u16(), 302, "round {round}");
        assert_eq!(
            response.headers().get("location").map(|v| v.to_str().ok()),
            Some(Some(location.as_str())),
            "round {round}: Location is passed through untouched"
        );
        assert_eq!(response.text().await.expect("body"), "moved");
    }

    let target_state = imposter(&client, &admin, target).await;
    assert_eq!(
        target_state["numberOfRequests"], 0,
        "the redirect target is never contacted: {target_state}"
    );

    let recorded = &imposter(&client, &admin, proxy).await["stubs"][0]["responses"][0]["is"];
    assert_eq!(recorded["statusCode"], 302, "{recorded}");
    let recorded_location = recorded["headers"]
        .as_object()
        .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")))
        .map(|(_, v)| v.clone());
    assert_eq!(recorded_location, Some(json!(location)), "{recorded}");

    for port in [proxy, origin, target] {
        let _ = manager.delete_imposter(port).await;
    }
}
