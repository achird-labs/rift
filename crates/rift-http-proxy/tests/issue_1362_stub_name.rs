//! Issue #1362: a stub's `name` is a label, kept and printed back as Mountebank prints it. It used
//! to be dropped on decode, so it vanished from every printed form and a `--datadir` persist erased
//! it from the operator's file.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

async fn start_server(
    datadir: Option<PathBuf>,
) -> (String, rift_http_proxy::admin_api::RunningAdminApi) {
    let manager = Arc::new(ImposterManager::with_datadir(datadir));
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let running = AdminApiServer::new(addr, manager, None)
        .bind()
        .await
        .expect("bind admin");
    (format!("http://{}", running.local_addr()), running)
}

fn named_imposter() -> Value {
    json!({
        "port": 0, "protocol": "http",
        "stubs": [{
            "name": "s1",
            "predicates": [{ "equals": { "path": "/a" } }],
            "responses": [{ "is": { "body": "A" } }]
        }]
    })
}

async fn get_json(client: &reqwest::Client, url: String) -> Value {
    let resp = client.get(&url).send().await.expect("send");
    assert_eq!(resp.status().as_u16(), 200, "{url}");
    resp.json().await.expect("JSON body")
}

#[tokio::test]
async fn a_stub_name_is_printed_by_every_admin_form() {
    let (admin, running) = start_server(None).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&named_imposter())
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    let created: Value = resp.json().await.expect("JSON body");
    assert_eq!(created["stubs"][0]["name"], "s1", "POST 201: {created}");
    let port = created["port"].as_u64().expect("assigned port");

    let got = get_json(&client, format!("{admin}/imposters/{port}")).await;
    assert_eq!(got["stubs"][0]["name"], "s1", "GET: {got}");

    let replayable = get_json(&client, format!("{admin}/imposters/{port}?replayable=true")).await;
    assert_eq!(
        replayable["stubs"][0]["name"], "s1",
        "replayable: {replayable}"
    );

    let without_proxies = get_json(
        &client,
        format!("{admin}/imposters/{port}?replayable=true&removeProxies=true"),
    )
    .await;
    assert_eq!(
        without_proxies["stubs"][0]["name"], "s1",
        "removeProxies: {without_proxies}"
    );

    let one = get_json(&client, format!("{admin}/imposters/{port}/stubs/0")).await;
    assert_eq!(one["name"], "s1", "GET stubs/0: {one}");

    running.shutdown().await;
}

#[tokio::test]
async fn a_stub_name_survives_a_datadir_persist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (admin, running) = start_server(Some(dir.path().to_path_buf())).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{admin}/imposters"))
        .json(&named_imposter())
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status().as_u16(), 201);
    let created: Value = resp.json().await.expect("JSON body");
    let port = created["port"].as_u64().expect("assigned port");

    let path = dir.path().join(format!("{port}.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let persisted: Value = serde_json::from_str(&text).expect("persisted JSON");
    assert_eq!(
        persisted["stubs"][0]["name"], "s1",
        "persisted: {persisted}"
    );

    running.shutdown().await;
}
