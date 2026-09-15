//! Issue #1101: a JSON array `_behaviors` was parsed positionally into `ResponseBehaviors`, so its
//! fifth element became a `shellTransform` and its sixth a `decorate`. The `--allowInjection` gate
//! only reads object keys and waved the array through. These tests drive the real doors a config
//! reaches the engine through, and require the shape to be refused at each one, whatever the flag.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

/// Fits every slot it fills: nulls for wait/repeat/copy/lookup, then a shellTransform string.
fn positional_shell_transform() -> serde_json::Value {
    json!([null, null, null, null, "echo pwned"])
}

async fn admin(allow_injection: bool) -> (rift_http_proxy::admin_api::RunningAdminApi, String) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .with_allow_injection(allow_injection)
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

#[tokio::test]
async fn post_imposters_refuses_an_array_underscore_behaviors_with_or_without_the_flag() {
    for allow_injection in [false, true] {
        let (running, base) = admin(allow_injection).await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({
                "protocol": "http",
                "stubs": [{ "responses": [{
                    "is": { "statusCode": 200 },
                    "_behaviors": positional_shell_transform()
                }] }]
            }))
            .send()
            .await
            .expect("POST /imposters");
        let status = response.status();
        let body = response.text().await.expect("body");

        assert_eq!(
            status, 400,
            "allowInjection={allow_injection}: an array `_behaviors` must be refused, body: {body}"
        );
        assert!(
            body.contains("_behaviors"),
            "allowInjection={allow_injection}: the refusal must name the key, body: {body}"
        );
        assert_eq!(
            imposter_count(&client, &base).await,
            0,
            "allowInjection={allow_injection}: nothing may be created"
        );
        running.shutdown().await;
    }
}

#[tokio::test]
async fn post_stubs_refuses_an_array_underscore_behaviors() {
    let (running, base) = admin(false).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters")
        .json()
        .await
        .expect("created imposter is JSON");
    let port = created["port"].as_u64().expect("assigned port");

    let response = client
        .post(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stub": { "responses": [{
            "is": { "statusCode": 200 },
            "_behaviors": positional_shell_transform()
        }] } }))
        .send()
        .await
        .expect("POST /imposters/:port/stubs");
    let status = response.status();
    let body = response.text().await.expect("body");
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("_behaviors"), "body: {body}");

    let imposter: serde_json::Value = client
        .get(format!("{base}/imposters/{port}"))
        .send()
        .await
        .expect("GET /imposters/:port")
        .json()
        .await
        .expect("imposter is JSON");
    assert_eq!(
        imposter["stubs"].as_array().map(Vec::len),
        Some(0),
        "no stub may be added: {imposter}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_an_array_underscore_behaviors_and_changes_nothing() {
    let (running, base) = admin(true).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);

    let response = client
        .put(format!("{base}/imposters"))
        .json(&json!({ "imposters": [{
            "protocol": "http",
            "stubs": [{ "responses": [{
                // The flat / recorded response form reads the same field.
                "statusCode": 200,
                "_behaviors": positional_shell_transform()
            }] }]
        }] }))
        .send()
        .await
        .expect("PUT /imposters");
    let status = response.status();
    let body = response.text().await.expect("body");
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("_behaviors"), "body: {body}");
    assert_eq!(
        imposter_count(&client, &base).await,
        1,
        "a refused batch must leave the running set alone"
    );
    running.shutdown().await;
}

#[test]
fn a_datadir_file_with_an_array_underscore_behaviors_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = json!({
        "protocol": "http",
        "stubs": [{ "responses": [{
            "is": { "statusCode": 200 },
            "_behaviors": positional_shell_transform()
        }] }]
    });
    std::fs::write(dir.path().join("4545.json"), config.to_string()).expect("write datadir file");

    let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
        .expect_err("an array `_behaviors` in a datadir file must fail the load");
    let message = format!("{err:#}");
    assert!(
        message.contains("_behaviors"),
        "the load error must name the key: {message}"
    );
}

#[test]
fn a_configfile_with_an_array_underscore_behaviors_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({ "imposters": [{
        "protocol": "http",
        "stubs": [{ "responses": [{
            "is": { "statusCode": 200 },
            "_behaviors": positional_shell_transform()
        }] }]
    }] });
    std::fs::write(&path, config.to_string()).expect("write config");

    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect_err("an array `_behaviors` must fail the load, not be admitted and executed");
    let message = format!("{err:#}");
    assert!(
        message.contains("_behaviors"),
        "the load error must name the key: {message}"
    );
}
