//! Issue #1314: a proxy's `mode` is parsed once, case-insensitively, and an omitted `mode` means
//! `proxyOnce` — Mountebank's default — for the replay store and the stub recorder alike. An
//! unknown mode is refused at the config door instead of silently becoming pass-through.

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

async fn post(client: &reqwest::Client, admin: &str, imposter: &Value) -> reqwest::Response {
    client
        .post(format!("{admin}/imposters"))
        .json(imposter)
        .send()
        .await
        .expect("POST")
}

async fn create(client: &reqwest::Client, admin: &str, imposter: Value) {
    let response = post(client, admin, &imposter).await;
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

async fn hits(client: &reqwest::Client, admin: &str, upstream: u16) -> usize {
    imposter(client, admin, upstream).await["requests"]
        .as_array()
        .map_or(0, Vec::len)
}

async fn body(client: &reqwest::Client, port: u16, path: &str) -> String {
    client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("proxied")
        .text()
        .await
        .expect("body")
}

#[tokio::test]
async fn an_omitted_mode_replays_like_proxy_once() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = free_port();
    create(
        &client,
        &admin,
        json!({"port": proxy, "protocol": "http", "stubs": [{"responses": [
            {"proxy": {"to": format!("http://127.0.0.1:{upstream}")}}
        ]}]}),
    )
    .await;

    let first = body(&client, proxy, "/x").await;
    let second = body(&client, proxy, "/x").await;
    let third = body(&client, proxy, "/x").await;
    assert_eq!(
        (first.as_str(), second.as_str(), third.as_str()),
        ("one", "one", "one")
    );
    assert_eq!(
        hits(&client, &admin, upstream).await,
        1,
        "replayed, like proxyOnce"
    );

    let cleared = client
        .delete(format!("{admin}/imposters/{proxy}/savedProxyResponses"))
        .send()
        .await
        .expect("DELETE");
    assert!(cleared.status().is_success());
    assert_eq!(body(&client, proxy, "/x").await, "two");
    assert_eq!(hits(&client, &admin, upstream).await, 2);
    manager.delete_all().await;
}

/// The half that already matched Mountebank: generators record a stub ahead of the proxy.
#[tokio::test]
async fn an_omitted_mode_with_generators_records_before_the_proxy_stub() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = free_port();
    create(
        &client,
        &admin,
        json!({"port": proxy, "protocol": "http", "stubs": [{"responses": [
            {"proxy": {"to": format!("http://127.0.0.1:{upstream}"),
                       "predicateGenerators": [{"matches": {"path": true}}]}}
        ]}]}),
    )
    .await;
    assert_eq!(body(&client, proxy, "/x").await, "one");
    assert_eq!(body(&client, proxy, "/x").await, "one");
    let stubs = imposter(&client, &admin, proxy).await["stubs"].clone();
    let stubs = stubs.as_array().expect("stubs");
    assert_eq!(stubs.len(), 2, "one recorded stub plus the proxy");
    assert!(
        stubs[0]["responses"][0].get("is").is_some(),
        "recorded stub first: {stubs:?}"
    );
    assert_eq!(hits(&client, &admin, upstream).await, 1);
    manager.delete_all().await;
}

/// `placement_for_mode` compared case-sensitively, so a lowercase `proxyalways` got a
/// proxyAlways store but proxyOnce placement.
#[tokio::test]
async fn proxy_always_in_any_casing_appends_after_the_proxy_stub() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let proxy = free_port();
    create(
        &client,
        &admin,
        json!({"port": proxy, "protocol": "http", "stubs": [{"responses": [
            {"proxy": {"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyalways",
                       "predicateGenerators": [{"matches": {"path": true}}]}}
        ]}]}),
    )
    .await;
    assert_eq!(body(&client, proxy, "/x").await, "one");
    assert_eq!(
        body(&client, proxy, "/x").await,
        "two",
        "proxyAlways keeps forwarding"
    );
    let stubs = imposter(&client, &admin, proxy).await["stubs"].clone();
    let stubs = stubs.as_array().expect("stubs");
    assert_eq!(stubs.len(), 2, "{stubs:?}");
    assert!(
        stubs[0]["responses"][0].get("proxy").is_some(),
        "proxy stub stays first"
    );
    assert_eq!(
        stubs[1]["responses"].as_array().map_or(0, Vec::len),
        2,
        "both responses appended to one recorded stub"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn an_unknown_proxy_mode_is_refused() {
    let (client, admin, manager) = start().await;
    let proxy = free_port();
    let config = json!({"port": proxy, "protocol": "http", "stubs": [{"responses": [
        {"proxy": {"to": "http://127.0.0.1:9", "mode": "bogus"}}
    ]}]});
    let response = post(&client, &admin, &config).await;
    assert_eq!(response.status().as_u16(), 400);
    let text = response.text().await.expect("body");
    assert!(
        text.contains("bogus") && text.contains("proxyOnce"),
        "{text}"
    );
    manager.delete_all().await;
}

#[test]
fn a_configfile_with_an_unknown_proxy_mode_fails_to_load() {
    use rift_http_proxy::config_loader::{ConfigSource, load_configs};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({ "imposters": [{ "port": 0, "protocol": "http", "stubs": [{
        "responses": [{ "proxy": { "to": "http://127.0.0.1:9", "mode": "bogus" } }]
    }] }] });
    std::fs::write(&path, config.to_string()).expect("write configfile");
    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect_err("an unknown proxy mode fails the load");
    assert!(format!("{err:#}").contains("bogus"), "{err:#}");
}

/// The store's mode is imposter-wide, so before #1314 a `defaultForward` (and a stub that was
/// itself `proxyTransparent`) replayed what another stub's proxyOnce had recorded. Now that an
/// omitted mode is proxyOnce, that would have hit every imposter mixing a proxy stub with
/// `defaultForward`.
#[tokio::test]
async fn default_forward_never_replays_beside_a_proxy_once_stub() {
    let (client, admin, manager) = start().await;
    let upstream = counting_upstream(&client, &admin).await;
    let to = format!("http://127.0.0.1:{upstream}");
    let proxy = free_port();
    create(
        &client,
        &admin,
        json!({"port": proxy, "protocol": "http", "defaultForward": to, "stubs": [
            {"predicates": [{"equals": {"path": "/p"}}], "responses": [{"proxy": {"to": to}}]},
            {"predicates": [{"equals": {"path": "/t"}}],
             "responses": [{"proxy": {"to": to, "mode": "proxyTransparent"}}]}
        ]}),
    )
    .await;
    assert_eq!(body(&client, proxy, "/x").await, "one");
    assert_eq!(
        body(&client, proxy, "/x").await,
        "two",
        "defaultForward forwards every time"
    );
    assert_eq!(body(&client, proxy, "/t").await, "three");
    assert_eq!(
        body(&client, proxy, "/t").await,
        "one",
        "a transparent stub forwards every time"
    );
    manager.delete_all().await;
}
