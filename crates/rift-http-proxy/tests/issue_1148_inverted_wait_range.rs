//! Issue #1148: a `wait` whose `min` exceeds its `max` was accepted at creation and then panicked
//! the tokio worker on *every* request to that stub — `gen_range` asserts a non-empty range, the
//! draw ran on the request task with no `catch_unwind`, so the connection was dropped and the stub
//! was permanently dead while the server stayed up and healthy.
//!
//! The refusal lives at the parse door (the #1109 layer), so these tests drive the real doors a
//! config reaches the engine through and require each one to refuse. `delayRange` is a second
//! spelling that is rewritten into a `wait` after parse, so it gets the same treatment.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

fn inverted_wait_stub() -> serde_json::Value {
    json!({ "responses": [{
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": { "min": 100, "max": 7 } }
    }] })
}

fn inverted_delay_range_stub() -> serde_json::Value {
    json!({
        "delayRange": [{ "min": 100, "max": 7 }],
        "responses": [{ "is": { "statusCode": 200 } }]
    })
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

#[tokio::test]
async fn post_imposters_refuses_an_inverted_wait_range() {
    for stub in [inverted_wait_stub(), inverted_delay_range_stub()] {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");

        assert_eq!(
            response.status(),
            400,
            "an inverted wait range must be refused at creation, not accepted and panicked later"
        );
        let body = response.text().await.expect("body");
        assert!(
            body.contains("100") && body.contains('7'),
            "the refusal names both bounds: {body}"
        );
        assert_eq!(
            imposter_count(&client, &base).await,
            0,
            "a refused imposter must not be registered"
        );
        running.shutdown().await;
    }
}

#[tokio::test]
async fn post_stubs_refuses_an_inverted_wait_range() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let response = client
        .post(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stub": inverted_wait_stub() }))
        .send()
        .await
        .expect("POST stubs");
    assert_eq!(
        response.status(),
        400,
        "the stub door must refuse it as well as the imposter door"
    );
    // The route takes `{"stub": …}`; without the wrapper this 400s on the envelope, and the test
    // passed without ever reaching the range check (found by #1162).
    let body = response.text().await.expect("body");
    assert!(
        body.contains("100") && body.contains('7'),
        "the refusal names both bounds: {body}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_an_inverted_wait_range_and_changes_nothing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let before = imposter_count(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters"))
        .json(&json!({ "imposters": [{ "protocol": "http", "stubs": [inverted_wait_stub()] }] }))
        .send()
        .await
        .expect("PUT /imposters");
    assert_eq!(response.status(), 400);
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused wholesale replace must leave the running set untouched"
    );
    running.shutdown().await;
}

#[test]
fn a_datadir_file_with_an_inverted_wait_range_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = json!({
        "port": 4545,
        "protocol": "http",
        "stubs": [inverted_wait_stub()]
    });
    std::fs::write(dir.path().join("4545.json"), config.to_string()).expect("write datadir file");

    let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
        .expect_err("an inverted wait range in a datadir file must fail the load");
    let message = format!("{err:#}");
    assert!(
        message.contains("wait"),
        "the load error must name the behavior: {message}"
    );
}

#[test]
fn a_configfile_with_an_inverted_delay_range_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({
        "imposters": [{ "port": 4546, "protocol": "http", "stubs": [inverted_delay_range_stub()] }]
    });
    std::fs::write(&path, config.to_string()).expect("write configfile");

    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect_err("an inverted delayRange in a configfile must fail the load");
    let message = format!("{err:#}");
    assert!(
        message.contains("100") && message.contains('7'),
        "the load error names both bounds: {message}"
    );
}

/// The stub-replace door. `Stub` carries the refusal in its deserializer, so this is structurally
/// covered — but "structurally covered" is an assumption until a test makes it an observation, and
/// this is the one by-index endpoint the other tests do not touch.
#[tokio::test]
async fn put_stub_by_index_refuses_an_inverted_wait_range() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();

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
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let response = client
        .put(format!("{base}/imposters/{port}/stubs/0"))
        .json(&inverted_wait_stub())
        .send()
        .await
        .expect("PUT stub by index");
    assert_eq!(
        response.status(),
        400,
        "the stub-replace door must refuse it too"
    );
    running.shutdown().await;
}

/// `POST /admin/reload` is the one door where the guarantee is a runtime reconciliation rather than
/// a parse-time `400`: the reload must be refused *and* leave the running imposters untouched. The
/// triage asked for this case by name.
#[tokio::test]
async fn reload_refuses_an_inverted_wait_range_and_leaves_the_running_set_unchanged() {
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

    // Rewrite the file with an inverted range, then ask the server to re-read it.
    let bad = json!({
        "imposters": [{
            "port": 0,
            "protocol": "http",
            "stubs": [inverted_wait_stub()]
        }]
    });
    std::fs::write(&path, bad.to_string()).expect("rewrite configfile");

    let response = client
        .post(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("POST /admin/reload");
    assert!(
        !response.status().is_success(),
        "a reload carrying an inverted wait range must be refused, got {}",
        response.status()
    );
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused reload must leave the running imposters exactly as they were"
    );
    running.shutdown().await;
}
