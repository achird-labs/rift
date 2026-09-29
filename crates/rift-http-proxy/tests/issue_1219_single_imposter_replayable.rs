//! Issue #1219: `GET /imposters/{port}?replayable=true` ignored `replayable` and returned the
//! detail view (`numberOfRequests`, `requests`, `_links`, `_rift`), while `GET
//! /imposters?replayable=true` and `rift_get_imposter(port, {"replayable":true})` returned the
//! imposter's config. Mountebank serves the same replayable document from both routes.
//!
//! While pinning the two routes to each other, the list route turned out to serve the stubs the
//! imposter was *created* with: a stub added afterwards (through the stub routes, or recorded by a
//! proxy) was missing from the export `rift save` writes. Both routes now project the live stubs.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    running: rift_http_proxy::admin_api::RunningAdminApi,
    base: String,
    port: u64,
    client: reqwest::Client,
}

/// An imposter with a plain stub and a pure-proxy stub, one recorded request, and a stub added
/// after creation — every part the replayable projection must drop, keep or filter.
async fn fixture() -> Fixture {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "recordRequests": true,
            "stubs": [
                { "id": "a", "responses": [{ "is": { "body": "A" } }] },
                { "id": "p", "responses": [{ "proxy": { "to": "http://127.0.0.1:9" } }] }
            ]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let served = client
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("request to the imposter");
    assert_eq!(served.text().await.expect("body"), "A");

    let added = client
        .post(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stub": { "id": "b", "responses": [{ "is": { "body": "B" } }] } }))
        .send()
        .await
        .expect("POST stubs");
    assert!(added.status().is_success(), "{}", added.status());

    Fixture {
        running,
        base,
        port,
        client,
    }
}

async fn get_json(client: &reqwest::Client, url: &str) -> Value {
    let response = client.get(url).send().await.expect("GET");
    assert_eq!(response.status(), 200, "GET {url}");
    response.json().await.expect("JSON body")
}

fn stub_ids(doc: &Value) -> Vec<&str> {
    doc["stubs"]
        .as_array()
        .expect("stubs is an array")
        .iter()
        .map(|s| s["id"].as_str().expect("stub id"))
        .collect()
}

/// The list route's entry for `port`.
fn entry_for(listing: &Value, port: u64) -> Value {
    listing["imposters"]
        .as_array()
        .expect("imposters is an array")
        .iter()
        .find(|i| i["port"] == port)
        .cloned()
        .expect("the imposter is listed")
}

#[tokio::test]
async fn single_imposter_replayable_is_the_list_routes_entry() {
    let f = fixture().await;

    let single = get_json(
        &f.client,
        &format!("{}/imposters/{}?replayable=true", f.base, f.port),
    )
    .await;
    for detail_only in ["numberOfRequests", "requests", "_links", "_rift", "enabled"] {
        assert!(
            single.get(detail_only).is_none(),
            "replayable must not carry `{detail_only}`: {single}"
        );
    }
    for stub in single["stubs"].as_array().expect("stubs") {
        assert!(stub.get("_links").is_none(), "no stub _links: {stub}");
        assert!(stub.get("matches").is_none(), "no stub matches: {stub}");
    }
    assert_eq!(
        stub_ids(&single),
        ["a", "p", "b"],
        "the live stubs, including one added after creation"
    );

    let listing = get_json(&f.client, &format!("{}/imposters?replayable=true", f.base)).await;
    assert_eq!(
        single,
        entry_for(&listing, f.port),
        "the single-imposter and list routes serve the same replayable document"
    );
    f.running.shutdown().await;
}

#[tokio::test]
async fn single_imposter_replayable_honours_remove_proxies() {
    let f = fixture().await;

    let single = get_json(
        &f.client,
        &format!(
            "{}/imposters/{}?replayable=true&removeProxies=true",
            f.base, f.port
        ),
    )
    .await;
    assert_eq!(
        stub_ids(&single),
        ["a", "b"],
        "the pure-proxy stub is dropped"
    );

    let listing = get_json(
        &f.client,
        &format!("{}/imposters?replayable=true&removeProxies=true", f.base),
    )
    .await;
    assert_eq!(single, entry_for(&listing, f.port));
    f.running.shutdown().await;
}

/// Without `replayable` the route keeps serving the detail view.
#[tokio::test]
async fn single_imposter_without_replayable_is_still_the_detail_view() {
    let f = fixture().await;

    let detail = get_json(&f.client, &format!("{}/imposters/{}", f.base, f.port)).await;
    assert_eq!(detail["numberOfRequests"], 1, "{detail}");
    assert!(detail.get("_links").is_some(), "{detail}");
    assert_eq!(detail["requests"].as_array().map(Vec::len), Some(1));
    f.running.shutdown().await;
}
