//! Issue #1372: a stub whose `responses` is absent or `[]` was admitted on every door and then, when
//! it matched, served the imposter's `defaultResponse`. Mountebank refuses both with
//! `'responses' must be a non-empty array` (`dryRunValidator.errorsForStub`), and so does Rift now,
//! as an admission check on every door. The `--datadir` half lives beside `read_and_parse_datadir`
//! in `src/server.rs`, which is private.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

const REFUSAL: &str = "'responses' must be a non-empty array";

/// The two shapes Mountebank refuses: no `responses` key, and an empty list.
fn responseless_stubs() -> Vec<serde_json::Value> {
    vec![
        json!({ "predicates": [] }),
        json!({ "predicates": [{ "equals": { "path": "/a" } }], "responses": [] }),
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

/// An imposter with one addressable stub (`id: "s1"`) answering `ok`.
async fn create_imposter(client: &reqwest::Client, base: &str) -> u64 {
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "id": "s1", "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port")
}

async fn stub_count(client: &reqwest::Client, base: &str, port: u64) -> usize {
    let imposter: serde_json::Value = client
        .get(format!("{base}/imposters/{port}"))
        .send()
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("imposter is JSON");
    imposter["stubs"].as_array().map_or(0, Vec::len)
}

async fn assert_refused(response: reqwest::Response, door: &str) {
    assert_eq!(
        response.status(),
        400,
        "{door} must refuse a stub without responses"
    );
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(body.contains(REFUSAL), "{door}: names the rule: {body}");
}

#[tokio::test]
async fn post_imposters_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    for stub in responseless_stubs() {
        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");
        assert_refused(response, "POST /imposters").await;
    }
    assert_eq!(imposter_count(&client, &base).await, 0);
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .put(format!("{base}/imposters"))
            .json(&json!({ "imposters": [{ "port": 0, "protocol": "http", "stubs": [stub] }] }))
            .send()
            .await
            .expect("PUT /imposters");
        assert_refused(response, "PUT /imposters").await;
    }
    assert_eq!(
        imposter_count(&client, &base).await,
        1,
        "a refused replace leaves the running set untouched"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn post_stub_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .post(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stub": stub }))
            .send()
            .await
            .expect("POST stubs");
        assert_refused(response, "POST /imposters/:port/stubs").await;
    }
    assert_eq!(stub_count(&client, &base, port).await, 1);
    running.shutdown().await;
}

#[tokio::test]
async fn put_stubs_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .put(format!("{base}/imposters/{port}/stubs"))
            .json(&json!({ "stubs": [stub] }))
            .send()
            .await
            .expect("PUT stubs");
        assert_refused(response, "PUT /imposters/:port/stubs").await;
    }
    assert_eq!(stub_count(&client, &base, port).await, 1);
    running.shutdown().await;
}

#[tokio::test]
async fn put_stub_by_index_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .put(format!("{base}/imposters/{port}/stubs/0"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub by index");
        assert_refused(response, "PUT /imposters/:port/stubs/0").await;
    }
    running.shutdown().await;
}

#[tokio::test]
async fn put_stub_by_id_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .put(format!("{base}/imposters/{port}/stubs/by-id/s1"))
            .json(&stub)
            .send()
            .await
            .expect("PUT stub by id");
        assert_refused(response, "PUT /imposters/:port/stubs/by-id/s1").await;
    }
    running.shutdown().await;
}

/// `{"predicates": []}` is shaped like a stub, so `not_a_stub_reason` lets it through (#932); the
/// decode then refuses it for having no responses. It used to be answered `201`.
#[tokio::test]
async fn space_stub_route_refuses_a_stub_without_responses() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    for stub in responseless_stubs() {
        let response = client
            .post(format!("{base}/imposters/{port}/spaces/flow-1/stubs"))
            .json(&stub)
            .send()
            .await
            .expect("POST space stub");
        assert_eq!(response.status(), 400, "space-stub route");
        let body = response.text().await.expect("body");
        assert!(body.contains(REFUSAL), "space-stub route: {body}");
    }
    assert_eq!(stub_count(&client, &base, port).await, 1);
    running.shutdown().await;
}

#[test]
fn config_file_refuses_a_stub_without_responses() {
    for stub in responseless_stubs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        let config =
            json!({ "imposters": [{ "port": 4501, "protocol": "http", "stubs": [stub] }] });
        std::fs::write(&path, config.to_string()).expect("write configfile");

        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("a stub without responses in a configfile must fail the load");
        let message = format!("{err:#}");
        assert!(message.contains(REFUSAL), "{message}");
    }
}

#[tokio::test]
async fn reload_refuses_a_stub_without_responses() {
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

    let bad = json!({ "imposters": [{ "port": 0, "protocol": "http", "stubs": [{ "predicates": [] }] }] });
    std::fs::write(&path, bad.to_string()).expect("rewrite configfile");
    let response = client
        .post(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("POST /admin/reload");
    assert!(!response.status().is_success(), "{}", response.status());
    let body = response.text().await.expect("body");
    assert!(body.contains(REFUSAL), "{body}");
    assert_eq!(imposter_count(&client, &base).await, before);
    running.shutdown().await;
}

/// Unchanged: a stub with one response serves it, and an imposter with no stubs is admitted
/// (Mountebank allows `"stubs": []`).
#[tokio::test]
async fn a_stub_with_a_response_and_an_imposter_without_stubs_still_load() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_imposter(&client, &base).await;
    let served = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("request to the imposter")
        .text()
        .await
        .expect("body");
    assert_eq!(served, "ok");

    let empty = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(empty.status(), 201);
    running.shutdown().await;
}

/// The migration the CHANGELOG gives: `"responses": [{"is": {}}]` keeps "match and serve the
/// default", because an empty `is` inherits the imposter's `defaultResponse` (#1369).
#[tokio::test]
async fn an_empty_is_response_serves_the_default_response() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({
            "port": 0,
            "protocol": "http",
            "defaultResponse": { "statusCode": 418, "body": "dflt" },
            "stubs": [{
                "predicates": [{ "equals": { "path": "/a" } }],
                "responses": [{ "is": {} }]
            }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let served = client
        .get(format!("http://127.0.0.1:{port}/a"))
        .send()
        .await
        .expect("request to the imposter");
    assert_eq!(served.status(), 418);
    assert_eq!(served.text().await.expect("body"), "dflt");
    running.shutdown().await;
}
