//! Issue #1321: a request body that is not UTF-8 is base64-encoded for the journal and predicates
//! (#636). The proxy used to forward that string, so the upstream received ASCII base64 instead of
//! the client's bytes, and the no-generator `proxyOnce` key hashed the string, so a text body equal
//! to a binary body's base64 shared its replay. The oracle is a Rift upstream with
//! `recordRequests`: its journal says how the bytes arrived (`_mode`) and how many there were
//! (`content-length`).

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

/// Not UTF-8 (`0xFF` can never start a sequence). Standard base64: `//4AAQLAwQ==`.
const BINARY: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x02, 0xC0, 0xC1];
const BINARY_BASE64: &str = "//4AAQLAwQ==";

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

/// A recording upstream that answers a different body on each hit, so a replay is visible.
async fn upstream(client: &reqwest::Client, admin: &str) -> u16 {
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

async fn proxy(client: &reqwest::Client, admin: &str, config: Value) -> u16 {
    let port = free_port();
    create(
        client,
        admin,
        json!({"port": port, "protocol": "http", "stubs": [{"responses": [{"proxy": config}]}]}),
    )
    .await;
    port
}

async fn post(client: &reqwest::Client, port: u16, body: &'static [u8]) -> String {
    client
        .post(format!("http://127.0.0.1:{port}/upload"))
        .body(body)
        .send()
        .await
        .expect("proxied")
        .text()
        .await
        .expect("body")
}

async fn requests(client: &reqwest::Client, admin: &str, port: u16) -> Vec<Value> {
    imposter(client, admin, port).await["requests"]
        .as_array()
        .expect("recordRequests journal present")
        .clone()
}

/// A journal header is a string, or a list when it repeated; its name keeps the sender's case.
fn header<'a>(request: &'a Value, name: &str) -> Option<&'a str> {
    let (_, value) = request["headers"]
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?;
    value.as_str().or_else(|| value[0].as_str())
}

/// How the upstream saw the body: `(_mode, body, content-length)`.
fn arrival(request: &Value) -> (Option<&str>, Option<&str>, Option<&str>) {
    (
        request["_mode"].as_str(),
        request["body"].as_str(),
        header(request, "content-length"),
    )
}

#[tokio::test]
async fn a_binary_body_reaches_the_upstream_byte_for_byte() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = proxy(
        &client,
        &admin,
        json!({"to": format!("http://127.0.0.1:{up}"), "mode": "proxyTransparent"}),
    )
    .await;

    post(&client, port, BINARY).await;

    let seen = requests(&client, &admin, up).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        arrival(&seen[0]),
        (Some("binary"), Some(BINARY_BASE64), Some("7")),
        "the upstream must receive the 7 client bytes, not 12 bytes of base64 text"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn default_forward_sends_the_bytes_too() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = free_port();
    create(
        &client,
        &admin,
        json!({"port": port, "protocol": "http", "defaultForward": format!("http://127.0.0.1:{up}")}),
    )
    .await;

    post(&client, port, BINARY).await;

    let seen = requests(&client, &admin, up).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        arrival(&seen[0]),
        (Some("binary"), Some(BINARY_BASE64), Some("7"))
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn a_text_body_spelling_a_binary_bodys_base64_is_a_different_request() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = proxy(
        &client,
        &admin,
        json!({"to": format!("http://127.0.0.1:{up}"), "mode": "proxyOnce"}),
    )
    .await;

    let text = post(&client, port, BINARY_BASE64.as_bytes()).await;
    let binary = post(&client, port, BINARY).await;

    assert_eq!((text.as_str(), binary.as_str()), ("one", "two"));
    assert_eq!(requests(&client, &admin, up).await.len(), 2);
    manager.delete_all().await;
}

#[tokio::test]
async fn a_text_body_is_forwarded_unchanged() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = proxy(
        &client,
        &admin,
        json!({"to": format!("http://127.0.0.1:{up}"), "mode": "proxyTransparent"}),
    )
    .await;

    post(&client, port, "héllo".as_bytes()).await;

    let seen = requests(&client, &admin, up).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(arrival(&seen[0]), (None, Some("héllo"), Some("6")));
    manager.delete_all().await;
}

#[tokio::test]
async fn the_same_binary_body_still_replays_without_generators() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = proxy(
        &client,
        &admin,
        json!({"to": format!("http://127.0.0.1:{up}"), "mode": "proxyOnce"}),
    )
    .await;

    let first = post(&client, port, BINARY).await;
    let second = post(&client, port, BINARY).await;

    assert_eq!((first.as_str(), second.as_str()), ("one", "one"));
    assert_eq!(requests(&client, &admin, up).await.len(), 1);
    manager.delete_all().await;
}

/// The generated `body` predicate is built from the string form, because the matcher compares
/// it against the next request's string form. Bytes there would never match a replay.
#[tokio::test]
async fn a_generated_body_predicate_uses_the_base64_form_and_replays() {
    let (client, admin, manager) = start().await;
    let up = upstream(&client, &admin).await;
    let port = proxy(
        &client,
        &admin,
        json!({
            "to": format!("http://127.0.0.1:{up}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{"matches": {"body": true}}]
        }),
    )
    .await;

    let first = post(&client, port, BINARY).await;
    let second = post(&client, port, BINARY).await;

    assert_eq!((first.as_str(), second.as_str()), ("one", "one"));
    assert_eq!(requests(&client, &admin, up).await.len(), 1);
    let stubs = imposter(&client, &admin, port).await["stubs"].clone();
    let recorded = stubs[0]["predicates"].to_string();
    assert!(
        recorded.contains(BINARY_BASE64),
        "generated predicate should carry the base64 body: {recorded}"
    );
    manager.delete_all().await;
}
