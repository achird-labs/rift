//! Issue #1229: a delete answers with the imposter **as it is at delete time**. `DELETE /imposters`
//! is Mountebank's "save before reset" idiom, so a stub added through `POST /imposters/{port}/stubs`
//! or swapped in with `PUT /imposters/{port}/stubs` must be in the returned document — it used to
//! carry the stubs the imposter was created with. `DELETE /imposters/{port}` had the same defect.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

async fn start_server() -> (
    String,
    Arc<ImposterManager>,
    rift_http_proxy::admin_api::RunningAdminApi,
) {
    let manager = Arc::new(ImposterManager::new());
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let running = AdminApiServer::new(addr, manager.clone(), None)
        .bind()
        .await
        .expect("bind admin");
    (format!("http://{}", running.local_addr()), manager, running)
}

fn stub(path: &str, body: &str) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }], "responses": [{ "is": { "body": body } }] })
}

async fn send(req: reqwest::RequestBuilder, expected: u16) -> Value {
    let resp = req.send().await.expect("send");
    assert_eq!(resp.status().as_u16(), expected);
    resp.json().await.expect("JSON body")
}

/// Create an imposter serving `/a`, then POST a `/b` stub onto it. Returns its port.
async fn imposter_with_an_added_stub(admin: &str, client: &reqwest::Client) -> u16 {
    let created = send(
        client
            .post(format!("{admin}/imposters"))
            .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub("/a", "A")] })),
        201,
    )
    .await;
    let port = created["port"].as_u64().expect("assigned port") as u16;
    send(
        client
            .post(format!("{admin}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub("/b", "B") })),
        200,
    )
    .await;
    port
}

fn stub_paths(imposter: &Value) -> Vec<String> {
    imposter["stubs"]
        .as_array()
        .unwrap_or_else(|| panic!("stubs array in {imposter}"))
        .iter()
        .map(|s| {
            s["predicates"][0]["equals"]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("path predicate in {s}"))
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn delete_all_returns_a_stub_added_after_creation() {
    let (admin, _manager, running) = start_server().await;
    let client = reqwest::Client::new();
    imposter_with_an_added_stub(&admin, &client).await;

    let body = send(client.delete(format!("{admin}/imposters")), 200).await;
    let imposters = body["imposters"].as_array().expect("imposters array");
    assert_eq!(imposters.len(), 1, "{body}");
    assert_eq!(stub_paths(&imposters[0]), ["/a", "/b"]);
    running.shutdown().await;
}

#[tokio::test]
async fn delete_all_returns_the_stubs_a_put_swapped_in() {
    let (admin, _manager, running) = start_server().await;
    let client = reqwest::Client::new();
    let port = imposter_with_an_added_stub(&admin, &client).await;
    send(
        client
            .put(format!("{admin}/imposters/{port}/stubs"))
            .json(&json!({ "stubs": [stub("/c", "C")] })),
        200,
    )
    .await;

    let body = send(client.delete(format!("{admin}/imposters")), 200).await;
    assert_eq!(stub_paths(&body["imposters"][0]), ["/c"]);
    running.shutdown().await;
}

/// The delete-all body is the replayable projection at delete time: the same document a
/// `GET /imposters?replayable=true` issued just before it lists (issue #1219's projection).
#[tokio::test]
async fn delete_all_matches_the_replayable_export_taken_just_before() {
    let (admin, _manager, running) = start_server().await;
    let client = reqwest::Client::new();
    imposter_with_an_added_stub(&admin, &client).await;

    let exported = send(
        client.get(format!("{admin}/imposters?replayable=true")),
        200,
    )
    .await;
    let deleted = send(client.delete(format!("{admin}/imposters")), 200).await;
    assert_eq!(deleted["imposters"], exported["imposters"]);
    running.shutdown().await;
}

#[tokio::test]
async fn delete_one_returns_a_stub_added_after_creation() {
    let (admin, _manager, running) = start_server().await;
    let client = reqwest::Client::new();
    let port = imposter_with_an_added_stub(&admin, &client).await;

    let body = send(client.delete(format!("{admin}/imposters/{port}")), 200).await;
    assert_eq!(stub_paths(&body), ["/a", "/b"]);
    // Each stub's `_links` points at its own index in the live list.
    assert_eq!(
        body["stubs"][1]["_links"]["self"]["href"]
            .as_str()
            .expect("stub link"),
        format!("{admin}/imposters/{port}/stubs/1")
    );
    running.shutdown().await;
}
