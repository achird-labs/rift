//! Issue #1504: an imposter with no `_rift.flowState` and no scenario, script or `stateOps` stub
//! used to sit on a no-op flow store, so admin flow-state and scenario writes answered success and
//! kept nothing. Every imposter now has a real in-memory store.

use std::sync::Arc;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;

async fn start() -> (String, RunningAdminApi) {
    let server = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    );
    let running = server.bind().await.expect("admin binds");
    (format!("http://{}", running.local_addr()), running)
}

async fn create_plain(client: &reqwest::Client, base: &str) -> u16 {
    let response = client
        .post(format!("{base}/imposters"))
        .json(&serde_json::json!({
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "plain" } }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status(), 201, "create imposter");
    let body: serde_json::Value = response.json().await.expect("json");
    u16::try_from(body["port"].as_u64().expect("port")).expect("u16")
}

async fn put_value(client: &reqwest::Client, url: String, value: i64) {
    let response = client
        .put(url)
        .json(&serde_json::json!({ "value": value }))
        .send()
        .await
        .expect("PUT flow-state");
    assert_eq!(response.status(), 200, "PUT flow-state");
}

async fn get_status(client: &reqwest::Client, url: String) -> (u16, String) {
    let response = client.get(url).send().await.expect("GET");
    (
        response.status().as_u16(),
        response.text().await.expect("body"),
    )
}

#[tokio::test]
async fn flow_state_round_trips_on_a_plain_imposter() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create_plain(&client, &base).await;
    let key_url = format!("{base}/admin/imposters/{port}/flow-state/t1/k");

    put_value(&client, key_url.clone(), 42).await;
    let (status, body) = get_status(&client, key_url.clone()).await;
    assert_eq!(status, 200, "GET after PUT: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value,
        serde_json::json!({ "flowId": "t1", "key": "k", "value": 42 })
    );

    let response = client.delete(key_url.clone()).send().await.expect("DELETE");
    assert_eq!(response.status(), 200, "DELETE key");
    let (status, body) = get_status(&client, key_url.clone()).await;
    assert_eq!(status, 404, "GET after DELETE key: {body}");

    let a = format!("{base}/admin/imposters/{port}/flow-state/t1/a");
    let b = format!("{base}/admin/imposters/{port}/flow-state/t1/b");
    put_value(&client, a.clone(), 1).await;
    put_value(&client, b.clone(), 2).await;
    let (status, body) = get_status(&client, a.clone()).await;
    assert_eq!(status, 200, "GET a before clear: {body}");
    let response = client
        .delete(format!("{base}/admin/imposters/{port}/flow-state/t1"))
        .send()
        .await
        .expect("DELETE flow");
    assert_eq!(response.status(), 200, "DELETE flow");
    for url in [a, b] {
        let (status, body) = get_status(&client, url.clone()).await;
        assert_eq!(status, 404, "GET {url} after DELETE flow: {body}");
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn scenario_stub_added_in_place_keeps_its_state() {
    let (base, admin) = start().await;
    let client = reqwest::Client::new();
    let port = create_plain(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters/{port}/stubs"))
        .json(&serde_json::json!({ "stubs": [
            { "scenarioName": "S1", "requiredScenarioState": "B",
              "responses": [{ "is": { "statusCode": 201, "body": "gated" } }] },
            { "responses": [{ "is": { "statusCode": 200, "body": "plain" } }] }
        ] }))
        .send()
        .await
        .expect("PUT stubs");
    assert_eq!(response.status(), 200, "PUT stubs");

    let response = client
        .put(format!("{base}/imposters/{port}/scenarios/S1/state"))
        .json(&serde_json::json!({ "state": "B" }))
        .send()
        .await
        .expect("PUT state");
    assert_eq!(response.status(), 200, "PUT scenario state");

    let (status, body) = get_status(&client, format!("{base}/imposters/{port}/scenarios")).await;
    assert_eq!(status, 200, "GET scenarios: {body}");
    let scenarios: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(scenarios["scenarios"][0]["name"], "S1", "{body}");
    assert_eq!(scenarios["scenarios"][0]["state"], "B", "{body}");

    let (status, body) = get_status(&client, format!("http://127.0.0.1:{port}/anything")).await;
    assert_eq!(status, 201, "request is answered by the gated stub: {body}");
    assert_eq!(body, "gated");
    admin.shutdown().await;
}
