//! Issue #1317: without `predicateGenerators` the proxy's replay store keys on method, path, query
//! string and request body (headers stay out), so two POSTs with different bodies are different
//! requests. With generators the user chose the identity: the generated predicates key it instead
//! (issue #1333), so a body the generators ignore is the same request.

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

/// An upstream that answers a different body each time, so a replay is visible.
async fn counting_upstream(client: &reqwest::Client, admin: &str) -> u16 {
    let port = free_port();
    create(
        client,
        admin,
        json!({"port": port, "protocol": "http", "recordRequests": true, "stubs": [{"responses": [
            {"is": {"body": "one"}}, {"is": {"body": "two"}}, {"is": {"body": "three"}}
        ]}]}),
    )
    .await;
    port
}

async fn proxy_once(
    client: &reqwest::Client,
    admin: &str,
    upstream: u16,
    generators: Value,
) -> u16 {
    let port = free_port();
    let mut proxy = json!({"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce"});
    if !generators.is_null() {
        proxy["predicateGenerators"] = generators;
    }
    create(
        client,
        admin,
        json!({"port": port, "protocol": "http", "stubs": [{"responses": [{"proxy": proxy}]}]}),
    )
    .await;
    port
}

async fn hits(client: &reqwest::Client, admin: &str, upstream: u16) -> usize {
    imposter(client, admin, upstream).await["requests"]
        .as_array()
        .map_or(0, Vec::len)
}

async fn send(client: &reqwest::Client, port: u16, method: &str, path: &str, body: &str) -> String {
    client
        .request(
            method.parse().expect("method"),
            format!("http://127.0.0.1:{port}{path}"),
        )
        .body(body.to_string())
        .send()
        .await
        .expect("proxied")
        .text()
        .await
        .expect("body")
}

#[tokio::test]
async fn different_post_bodies_are_different_requests_without_generators() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = proxy_once(&client, &admin, upstream, Value::Null).await;

    let a = send(&client, proxy, "POST", "/orders", "body-a").await;
    let b = send(&client, proxy, "POST", "/orders", "body-b").await;
    assert_eq!((a.as_str(), b.as_str()), ("one", "two"));
    assert_eq!(hits(&client, &admin, upstream).await, 2);
    let requests = imposter(&client, &admin, upstream).await["requests"].clone();
    assert_eq!(requests[0]["body"], "body-a");
    assert_eq!(requests[1]["body"], "body-b");
    manager.delete_all().await;
}

#[tokio::test]
async fn the_same_post_body_still_replays() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = proxy_once(&client, &admin, upstream, Value::Null).await;

    let first = send(&client, proxy, "POST", "/orders", "body-a").await;
    let second = send(&client, proxy, "POST", "/orders", "body-a").await;
    assert_eq!((first.as_str(), second.as_str()), ("one", "one"));
    assert_eq!(hits(&client, &admin, upstream).await, 1);
    manager.delete_all().await;
}

#[tokio::test]
async fn a_body_less_request_key_is_unchanged() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = proxy_once(&client, &admin, upstream, Value::Null).await;

    send(&client, proxy, "GET", "/x", "").await;
    send(&client, proxy, "GET", "/x", "").await;
    assert_eq!(hits(&client, &admin, upstream).await, 1);

    send(&client, proxy, "GET", "/x?a=1", "").await;
    send(&client, proxy, "GET", "/x?a=2", "").await;
    assert_eq!(hits(&client, &admin, upstream).await, 3);
    manager.delete_all().await;
}

#[tokio::test]
async fn generators_keep_their_own_identity() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = proxy_once(
        &client,
        &admin,
        upstream,
        json!([{"matches": {"path": true}}]),
    )
    .await;

    let a = send(&client, proxy, "POST", "/orders", "body-a").await;
    let b = send(&client, proxy, "POST", "/orders", "body-b").await;
    assert_eq!((a.as_str(), b.as_str()), ("one", "one"));
    assert_eq!(hits(&client, &admin, upstream).await, 1);
    let stubs = imposter(&client, &admin, proxy).await["stubs"].clone();
    assert_eq!(
        stubs.as_array().expect("stubs").len(),
        2,
        "one recorded stub plus the proxy"
    );
    manager.delete_all().await;
}
