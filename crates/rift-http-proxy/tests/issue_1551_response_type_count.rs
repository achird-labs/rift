//! Issue #1551: a stub response with more than one of `is`, `proxy`, `inject` and `fault` kept the
//! first by precedence and dropped the rest, and one with none (`{}`, a typo'd key, only `behaviors`)
//! was served as an empty `200`. Mountebank refuses both on every config door, with
//! `each response object must have only one response type` and `unrecognized response type`; so
//! does Rift now. The flat form and a `_rift`-only response stay accepted (`compat_rift_only.rs`).

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const MULTIPLE_TYPES: &str = "each response object must have only one response type";
const NO_TYPE: &str = "unrecognized response type";

/// A response with two types and one with none, each with the message Mountebank answers.
fn refused_responses() -> Vec<(serde_json::Value, &'static str)> {
    vec![
        (
            json!({ "is": {}, "proxy": { "to": "http://127.0.0.1:9" } }),
            MULTIPLE_TYPES,
        ),
        (json!({}), NO_TYPE),
    ]
}

fn stub_with(response: &serde_json::Value) -> serde_json::Value {
    json!({ "responses": [response] })
}

async fn admin() -> (rift_http_proxy::admin_api::RunningAdminApi, String) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

async fn imposter_count(client: &reqwest::Client, base: &str) -> usize {
    let listing: serde_json::Value = client
        .get(format!("{base}/imposters"))
        .send()
        .await
        .expect("GET /imposters")
        .json()
        .await
        .expect("imposter listing is JSON");
    listing["imposters"]
        .as_array()
        .expect("imposters is an array")
        .len()
}

/// An imposter with one stub answering `ok`.
async fn create_imposter(client: &reqwest::Client, base: &str) -> u64 {
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port")
}

async fn served(client: &reqwest::Client, port: u64) -> String {
    client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("request to the imposter")
        .text()
        .await
        .expect("body")
}

async fn assert_refused(response: reqwest::Response, door: &str, refusal: &str) {
    assert_eq!(response.status(), 400, "{door} must refuse: {refusal}");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(body.contains(refusal), "{door}: names the rule: {body}");
}

#[tokio::test]
async fn post_imposters_refuses_two_types_or_none() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    for (response, refusal) in refused_responses() {
        let answer = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub_with(&response)] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_refused(answer, "POST /imposters", refusal).await;
    }
    assert_eq!(imposter_count(&client, &base).await, 0);
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_two_types_or_none() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for (response, refusal) in refused_responses() {
        let answer = client
            .put(format!("{base}/imposters"))
            .json(&json!({ "imposters": [{ "port": 0, "protocol": "http",
                                           "stubs": [stub_with(&response)] }] }))
            .send()
            .await
            .expect("PUT /imposters");
        assert_refused(answer, "PUT /imposters", refusal).await;
    }
    assert_eq!(served(&client, port).await, "ok", "the running set is kept");
    running.shutdown().await;
}

#[tokio::test]
async fn the_stub_doors_refuse_two_types_or_none() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for (response, refusal) in refused_responses() {
        let stub = stub_with(&response);
        let answer = client
            .post(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub }))
            .send()
            .await
            .expect("POST stubs");
        assert_refused(answer, "POST /imposters/:port/stubs", refusal).await;

        let answer = client
            .put(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stubs": [stub] }))
            .send()
            .await
            .expect("PUT stubs");
        assert_refused(answer, "PUT /imposters/:port/stubs", refusal).await;

        let answer = client
            .put(format!("{base}/imposters/{port}/stubs/0"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub by index");
        assert_refused(answer, "PUT /imposters/:port/stubs/0", refusal).await;
    }
    assert_eq!(served(&client, port).await, "ok", "the stub is unchanged");
    running.shutdown().await;
}

#[test]
fn config_file_refuses_two_types_or_none() {
    for (response, refusal) in refused_responses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        let config = json!({ "imposters": [{ "port": 0, "protocol": "http",
                                             "stubs": [stub_with(&response)] }] });
        std::fs::write(&path, config.to_string()).expect("write configfile");

        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("the configfile load must fail");
        let message = format!("{err:#}");
        assert!(message.contains(refusal), "{message}");
    }
}

#[tokio::test]
async fn reload_refuses_two_types_or_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let good = json!({ "imposters": [{
        "port": 0, "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
    }] });
    std::fs::write(&path, good.to_string()).expect("write configfile");

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .with_config_source(ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        })
        .bind()
        .await
        .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();
    let before = imposter_count(&client, &base).await;

    for (response, refusal) in refused_responses() {
        let bad = json!({ "imposters": [{ "port": 0, "protocol": "http",
                                          "stubs": [stub_with(&response)] }] });
        std::fs::write(&path, bad.to_string()).expect("rewrite configfile");
        let answer = client
            .post(format!("{base}/admin/reload"))
            .send()
            .await
            .expect("POST /admin/reload");
        assert!(!answer.status().is_success(), "{}", answer.status());
        let body = answer.text().await.expect("body");
        assert!(body.contains(refusal), "{body}");
        assert_eq!(imposter_count(&client, &base).await, before);
    }
    running.shutdown().await;
}

/// The migration the CHANGELOG gives for a behaviors-only response: an explicit `is: {}` beside the
/// block serves the delayed empty `200` it always meant.
#[tokio::test]
async fn an_explicit_empty_is_with_behaviors_still_serves() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": {}, "behaviors": [{ "wait": 10 }] }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let answer = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("request");
    assert_eq!(answer.status(), 200);
    running.shutdown().await;
}
