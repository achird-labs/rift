//! Issue #1729: a response with `is` beside `_rift.script` is refused at the admin door as two
//! response types. Its script never ran: the `is` was served.
//!
//! Every imposter takes an engine-assigned port, so none of these tests owns a fixed port.

use std::sync::Arc;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn admin() -> (String, RunningAdminApi) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .with_allow_injection(true)
    .bind()
    .await
    .expect("admin API binds");
    (format!("http://{}", running.local_addr()), running)
}

async fn post(url: &str, imposter: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{url}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST");
    let status = response.status().as_u16();
    (status, response.json().await.expect("json"))
}

fn script() -> Value {
    json!({"script": {"engine": "rhai", "code": "fn respond(ctx) { pass() }"}})
}

#[tokio::test]
async fn admin_post_refuses_is_beside_a_rift_script() {
    let (url, _running) = admin().await;
    let (status, body) = post(
        &url,
        json!({"protocol": "http", "stubs": [{"responses": [
            {"is": {"statusCode": 200, "body": "success"}, "_rift": script()}
        ]}]}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let message = body["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("each response object must have only one response type"),
        "{body}"
    );
}

#[tokio::test]
async fn admin_post_still_admits_a_bare_rift_script_and_is_with_other_rift_keys() {
    let (url, _running) = admin().await;
    let (status, body) = post(
        &url,
        json!({"protocol": "http", "stubs": [
            {"responses": [{"_rift": script()}]},
            {"responses": [{"is": {"statusCode": 200}, "_rift": {"fault": {"latency": {"probability": 1.0, "ms": 5}}}}]}
        ]}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

// A stored config an earlier engine admitted (a `--datadir` file, a rift-cluster replay) still loads
// through the replayed decode, and `GET /imposters/:port` says the script has no effect.
#[tokio::test]
async fn a_replayed_is_beside_a_rift_script_loads_and_is_reported_ignored() {
    let manager = Arc::new(ImposterManager::new());
    let config: rift_http_proxy::imposter::ImposterConfig =
        rift_http_proxy::imposter::deserialize_replayed(json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "success"}, "_rift": script()}]}]
        }))
        .expect("a stored config replays");
    let port = manager.create_imposter(config).await.expect("create");
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::clone(&manager),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let body: Value = reqwest::get(format!("http://{}/imposters/{port}", running.local_addr()))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let warnings = body["_rift"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w["warningType"] == "config_key_ignored"
                && w["message"].as_str().is_some_and(
                    |m| m.starts_with("`_rift.script` beside an `is` response has no effect")
                )),
        "{body}"
    );
    let served = reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert_eq!(
        served, "success",
        "the `is` is served; the script never runs"
    );
    manager.delete_all().await;
}
