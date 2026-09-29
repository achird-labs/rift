//! Issue #1220 (and the original #181): a `jsonpath`/`xpath` predicate selector that does not
//! compile was accepted with `201`, and at match time the failed extraction was read as the empty
//! string — so `{"equals":{"body":""}}` behind it matched *every* request.
//!
//! #181 was closed by #186 without its fix ever landing (#186's "closes #181" line belonged to the
//! `DELETE /imposters/:port` 404 change), which is why nothing caught it: there was never a test.
//! These tests are that test — they drive every door a config reaches the engine through and
//! require each one to refuse the selector by name.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const BAD_JSONPATH: &str = "$[[[bad";
const BAD_XPATH: &str = "//*[[";

fn jsonpath_stub(selector: &str) -> serde_json::Value {
    json!({
        "predicates": [{ "equals": { "body": "" }, "jsonpath": { "selector": selector } }],
        "responses": [{ "is": { "statusCode": 200, "body": "MATCHED" } }]
    })
}

fn xpath_stub(selector: &str) -> serde_json::Value {
    json!({
        "predicates": [{ "equals": { "body": "" }, "xpath": { "selector": selector } }],
        "responses": [{ "is": { "statusCode": 200, "body": "MATCHED" } }]
    })
}

/// The malformed selector buried under `not` → `and`, so the check is known to walk nested
/// predicates rather than only the top level.
fn nested_jsonpath_stub(selector: &str) -> serde_json::Value {
    json!({
        "predicates": [{ "not": { "and": [
            { "equals": { "method": "GET" } },
            { "equals": { "body": "" }, "jsonpath": { "selector": selector } }
        ] } }],
        "responses": [{ "is": { "statusCode": 200 } }]
    })
}

/// Every malformed shape, paired with the selector its refusal must name.
fn bad_stubs() -> Vec<(serde_json::Value, &'static str)> {
    vec![
        (jsonpath_stub(BAD_JSONPATH), BAD_JSONPATH),
        (xpath_stub(BAD_XPATH), BAD_XPATH),
        (nested_jsonpath_stub(BAD_JSONPATH), BAD_JSONPATH),
    ]
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

async fn create_empty_imposter(client: &reqwest::Client, base: &str) -> u64 {
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port")
}

/// Asserts a `400` whose error envelope is `bad data` and names `selector`.
async fn assert_bad_data(response: reqwest::Response, selector: &str, door: &str) {
    assert_eq!(
        response.status(),
        400,
        "{door} must refuse a malformed selector, not accept it and match everything"
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(
        body.contains(selector),
        "{door}: the refusal names the selector `{selector}`: {body}"
    );
}

#[tokio::test]
async fn post_imposters_refuses_a_malformed_selector() {
    for (stub, selector) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_bad_data(response, selector, "POST /imposters").await;
        assert_eq!(
            imposter_count(&client, &base).await,
            0,
            "a refused imposter must not be registered"
        );
        running.shutdown().await;
    }
}

/// The refusal must not over-reach: bare (Mountebank-style) and rooted JSONPath, a slice, and a
/// namespaced XPath all still load.
#[tokio::test]
async fn post_imposters_still_accepts_well_formed_selectors() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let stub = |pred: serde_json::Value| json!({ "predicates": [pred], "responses": [{ "is": { "statusCode": 200 } }] });

    let response = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "protocol": "http", "stubs": [
            stub(json!({ "equals": { "body": "1" }, "jsonpath": { "selector": "$.a" } })),
            stub(json!({ "equals": { "body": "1" }, "jsonpath": { "selector": "user.name" } })),
            stub(json!({ "equals": { "body": "1" }, "jsonpath": { "selector": "$.items[:2]" } })),
            stub(json!({ "equals": { "body": "1" },
                          "xpath": { "selector": "//a:user", "ns": { "a": "urn:y" } } })),
        ] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        response.status(),
        201,
        "{}",
        response.text().await.unwrap_or_default()
    );
    running.shutdown().await;
}

#[tokio::test]
async fn stub_doors_refuse_a_malformed_selector() {
    for (stub, selector) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();
        let port = create_empty_imposter(&client, &base).await;

        let response = client
            .post(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub.clone() }))
            .send()
            .await
            .expect("POST stubs");
        assert_bad_data(response, selector, "POST /imposters/:port/stubs").await;

        let response = client
            .put(format!("{base}/imposters/{port}/stubs/0"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub by index");
        assert_bad_data(response, selector, "PUT /imposters/:port/stubs/0").await;

        let response = client
            .put(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stubs": [stub] }))
            .send()
            .await
            .expect("PUT stubs");
        assert_bad_data(response, selector, "PUT /imposters/:port/stubs").await;
        running.shutdown().await;
    }
}

#[tokio::test]
async fn put_imposters_refuses_a_malformed_selector_and_changes_nothing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    create_empty_imposter(&client, &base).await;
    let before = imposter_count(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters"))
        .json(&json!({ "imposters": [{ "protocol": "http", "stubs": [xpath_stub(BAD_XPATH)] }] }))
        .send()
        .await
        .expect("PUT /imposters");
    assert_bad_data(response, BAD_XPATH, "PUT /imposters").await;
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused wholesale replace must leave the running set untouched"
    );
    running.shutdown().await;
}

#[test]
fn a_configfile_with_a_malformed_selector_fails_to_load() {
    for (stub, selector) in bad_stubs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        let config =
            json!({ "imposters": [{ "port": 4547, "protocol": "http", "stubs": [stub] }] });
        std::fs::write(&path, config.to_string()).expect("write configfile");

        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("a malformed selector in a configfile must fail the load");
        let message = format!("{err:#}");
        assert!(
            message.contains(selector),
            "the load error names the selector `{selector}`: {message}"
        );
    }
}

#[tokio::test]
async fn reload_refuses_a_malformed_selector_and_leaves_the_running_set_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let good = json!({
        "imposters": [{
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "v1" } }] }]
        }]
    });
    std::fs::write(&path, good.to_string()).expect("write configfile");

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
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

    let bad = json!({
        "imposters": [{ "port": 0, "protocol": "http", "stubs": [jsonpath_stub(BAD_JSONPATH)] }]
    });
    std::fs::write(&path, bad.to_string()).expect("rewrite configfile");

    let response = client
        .post(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("POST /admin/reload");
    assert!(
        !response.status().is_success(),
        "a reload carrying a malformed selector must be refused, got {}",
        response.status()
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains(BAD_JSONPATH),
        "the reload refusal names the selector: {body}"
    );
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused reload must leave the running imposters exactly as they were"
    );
    running.shutdown().await;
}
