//! Issue #1722: a flat response (`statusCode`/`headers`/`body` at the top level) carrying a `_rift`
//! block without a script is that `is` response with the block's effects. It decoded as a
//! script-less `_rift` response instead: answered as an empty `200`, and its flat fields dropped
//! from `GET /imposters` and `--datadir`.
//!
//! Every imposter takes an engine-assigned port, so none of these tests owns a fixed port.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn admin() -> (String, RunningAdminApi) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    (format!("http://{}", running.local_addr()), running)
}

async fn create(url: &str, response: Value) -> u16 {
    let reply = reqwest::Client::new()
        .post(format!("{url}/imposters"))
        .json(&json!({"protocol": "http", "stubs": [{"responses": [response]}]}))
        .send()
        .await
        .expect("POST");
    let status = reply.status().as_u16();
    let body: Value = reply.json().await.expect("json");
    assert_eq!(status, 201, "{body}");
    body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("assigned port")
}

#[tokio::test]
async fn a_flat_response_with_a_latency_fault_serves_its_status_after_the_delay() {
    let (url, _running) = admin().await;
    let port = create(
        &url,
        json!({"statusCode": 503, "headers": {"X-A": "1"}, "body": "down",
               "_rift": {"fault": {"latency": {"probability": 1.0, "ms": 300}}}}),
    )
    .await;
    let started = Instant::now();
    let reply = reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .expect("request");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the fault's delay applies"
    );
    assert_eq!(reply.status().as_u16(), 503);
    assert_eq!(reply.headers()["x-a"], "1");
    assert_eq!(reply.text().await.expect("body"), "down");
}

#[tokio::test]
async fn a_flat_response_with_templated_renders_its_body_and_runs_its_behaviors() {
    let (url, _running) = admin().await;
    let port = create(
        &url,
        json!({"statusCode": 201, "body": "path=${request.path}",
               "_rift": {"templated": true},
               "_behaviors": {"wait": 200}}),
    )
    .await;
    let started = Instant::now();
    let reply = reqwest::get(format!("http://127.0.0.1:{port}/abc"))
        .await
        .expect("request");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the wait runs"
    );
    assert_eq!(reply.status().as_u16(), 201);
    assert_eq!(reply.text().await.expect("body"), "path=/abc");
}

#[tokio::test]
async fn a_flat_response_with_rift_round_trips_in_the_is_form() {
    let (url, _running) = admin().await;
    let port = create(
        &url,
        json!({"statusCode": 503, "body": "down",
               "_rift": {"fault": {"latency": {"probability": 1.0, "ms": 5}}}}),
    )
    .await;
    let body: Value = reqwest::get(format!("{url}/imposters/{port}"))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let response = &body["stubs"][0]["responses"][0];
    assert_eq!(response["is"]["statusCode"], 503, "{response}");
    assert_eq!(response["is"]["body"], "down", "{response}");
    assert!(response["_rift"]["fault"].is_object(), "{response}");
}
