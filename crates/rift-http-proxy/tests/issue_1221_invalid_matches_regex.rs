//! Issue #1221: a `matches` predicate whose regex does not compile (`{"matches":{"path":"(["}}`) was
//! accepted with `201`; the matcher then treated the unparseable pattern as "no match", so the stub
//! was silently dead and the client had been told it worked.
//!
//! Refused at the same parse door as a malformed `jsonpath`/`xpath` selector (#1220), so these tests
//! drive every door a config reaches the engine through and require each to refuse by name.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const BAD_REGEX: &str = "([";

fn stub_with(predicate: serde_json::Value) -> serde_json::Value {
    json!({
        "predicates": [predicate],
        "responses": [{ "is": { "statusCode": 200, "body": "MATCHED" } }]
    })
}

/// Every place a `matches` pattern can sit, paired with the field its refusal must name: a plain
/// field, a keyed field (`query`), a JSON-body object leaf, and a pattern nested under `or` → `not`.
fn bad_stubs() -> Vec<(serde_json::Value, &'static str)> {
    vec![
        (
            stub_with(json!({ "matches": { "path": BAD_REGEX } })),
            "path",
        ),
        (
            stub_with(json!({ "matches": { "query": { "q": BAD_REGEX } } })),
            "query",
        ),
        (
            stub_with(json!({ "matches": { "body": { "user": { "name": BAD_REGEX } } } })),
            "body",
        ),
        (
            stub_with(json!({ "or": [
                { "equals": { "method": "PUT" } },
                { "not": { "matches": { "path": BAD_REGEX } } }
            ] })),
            "path",
        ),
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

async fn create_imposter(client: &reqwest::Client, base: &str) -> u64 {
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

/// Asserts a `400` `bad data` refusal naming both the `matches` field and the pattern.
async fn assert_bad_data(response: reqwest::Response, field: &str, door: &str) {
    assert_eq!(
        response.status(),
        400,
        "{door} must refuse an invalid regex, not accept a stub that can never match"
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(
        body.contains(&format!("`{field}`")) && body.contains(BAD_REGEX),
        "{door}: the refusal names the field `{field}` and the pattern: {body}"
    );
}

#[tokio::test]
async fn post_imposters_refuses_an_invalid_matches_regex() {
    for (stub, field) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_bad_data(response, field, "POST /imposters").await;
        assert_eq!(
            imposter_count(&client, &base).await,
            0,
            "a refused imposter must not be registered"
        );
        running.shutdown().await;
    }
}

/// The refusal must not over-reach: valid patterns (including a case-insensitive one, the default,
/// and a `caseSensitive` one) still load and still match.
#[tokio::test]
async fn valid_matches_regexes_still_load_and_match() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [
            stub_with(json!({ "matches": { "path": "^/users/\\d+$", "query": { "q": "a.c" } } })),
            stub_with(json!({ "caseSensitive": true, "matches": { "body": { "n": "^[A-Z]+$" } } })),
        ] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let served = client
        .get(format!("http://127.0.0.1:{port}/users/42?q=abc"))
        .send()
        .await
        .expect("request to the imposter")
        .text()
        .await
        .expect("body");
    assert_eq!(served, "MATCHED");
    running.shutdown().await;
}

#[tokio::test]
async fn stub_doors_refuse_an_invalid_matches_regex() {
    for (stub, field) in bad_stubs() {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();
        let port = create_imposter(&client, &base).await;

        let response = client
            .post(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub.clone() }))
            .send()
            .await
            .expect("POST stubs");
        assert_bad_data(response, field, "POST /imposters/:port/stubs").await;

        let response = client
            .put(format!("{base}/imposters/{port}/stubs/0"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub by index");
        assert_bad_data(response, field, "PUT /imposters/:port/stubs/0").await;

        let response = client
            .put(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stubs": [stub] }))
            .send()
            .await
            .expect("PUT stubs");
        assert_bad_data(response, field, "PUT /imposters/:port/stubs").await;
        running.shutdown().await;
    }
}

#[tokio::test]
async fn put_imposters_refuses_an_invalid_matches_regex_and_changes_nothing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    create_imposter(&client, &base).await;
    let before = imposter_count(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters"))
        .json(&json!({ "imposters": [{
            "protocol": "http",
            "stubs": [stub_with(json!({ "matches": { "path": BAD_REGEX } }))]
        }] }))
        .send()
        .await
        .expect("PUT /imposters");
    assert_bad_data(response, "path", "PUT /imposters").await;
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused wholesale replace must leave the running set untouched"
    );
    running.shutdown().await;
}

#[test]
fn a_configfile_with_an_invalid_matches_regex_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({ "imposters": [{
        "port": 4548,
        "protocol": "http",
        "stubs": [stub_with(json!({ "matches": { "path": BAD_REGEX } }))]
    }] });
    std::fs::write(&path, config.to_string()).expect("write configfile");

    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect_err("an invalid matches regex in a configfile must fail the load");
    let message = format!("{err:#}");
    assert!(
        message.contains("`path`") && message.contains(BAD_REGEX),
        "the load error names the field and the pattern: {message}"
    );
}

#[tokio::test]
async fn reload_refuses_an_invalid_matches_regex_and_leaves_the_running_set_unchanged() {
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

    let bad = json!({ "imposters": [{
        "port": 0,
        "protocol": "http",
        "stubs": [stub_with(json!({ "matches": { "path": BAD_REGEX } }))]
    }] });
    std::fs::write(&path, bad.to_string()).expect("rewrite configfile");

    let response = client
        .post(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("POST /admin/reload");
    assert!(
        !response.status().is_success(),
        "a reload carrying an invalid regex must be refused, got {}",
        response.status()
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains(BAD_REGEX),
        "the reload refusal names the pattern: {body}"
    );
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused reload must leave the running imposters exactly as they were"
    );
    running.shutdown().await;
}
