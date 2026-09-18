//! Issue #1162: a behaviors block the executor cannot parse — a `wait` with numeric-string bounds, a
//! fractional `repeat`, a `copy` with no `using` — was admitted, and the stub served with every
//! behavior but `repeat` ignored and only a server-side log line to say so. With `--allowInjection`
//! off, a non-numeric `wait` was refused instead, but as an *injection* error, which misdiagnoses a
//! quoting mistake as a scripting attempt.
//!
//! The refusal lives where the response is parsed, so these tests drive the doors a config reaches
//! the engine through (the #1148 matrix) and require each one to refuse, naming the key.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

/// Numeric-string bounds: the shape the issue was filed for.
fn string_bound_stub() -> serde_json::Value {
    json!({ "responses": [{
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": { "min": "100", "max": "200" } }
    }] })
}

/// No script surface at all, so the injection gate waved it through and it loaded degraded.
fn fractional_repeat_stub() -> serde_json::Value {
    json!({ "responses": [{
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": 100, "repeat": 2.0 }
    }] })
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

#[tokio::test]
async fn post_imposters_refuses_an_unparseable_block_naming_the_key() {
    for (stub, key) in [
        (string_bound_stub(), "`wait`"),
        (fractional_repeat_stub(), "`repeat`"),
    ] {
        let (running, base) = admin().await;
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{base}/imposters"))
            .json(&json!({ "protocol": "http", "stubs": [stub] }))
            .send()
            .await
            .expect("POST /imposters");

        assert_eq!(response.status(), 400, "{key} must be refused at creation");
        let body = response.text().await.expect("body");
        assert!(body.contains(key), "the refusal names {key}: {body}");
        assert!(
            !body.contains("allowInjection"),
            "a malformed delay is not an injection problem: {body}"
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
async fn post_stubs_refuses_an_unparseable_block() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_empty_imposter(&client, &base).await;

    let response = client
        .post(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stub": fractional_repeat_stub() }))
        .send()
        .await
        .expect("POST stubs");
    assert_eq!(response.status(), 400, "the stub door must refuse it too");
    let body = response.text().await.expect("body");
    assert!(body.contains("`repeat`"), "{body}");
    running.shutdown().await;
}

#[tokio::test]
async fn put_stub_by_index_refuses_an_unparseable_block() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let port = create_empty_imposter(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters/{port}/stubs/0"))
        .json(&fractional_repeat_stub())
        .send()
        .await
        .expect("PUT stub by index");
    assert_eq!(response.status(), 400);
    let body = response.text().await.expect("body");
    assert!(body.contains("`repeat`"), "{body}");
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_an_unparseable_block_and_changes_nothing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    create_empty_imposter(&client, &base).await;
    let before = imposter_count(&client, &base).await;

    let response = client
        .put(format!("{base}/imposters"))
        .json(
            &json!({ "imposters": [{ "protocol": "http", "stubs": [fractional_repeat_stub()] }] }),
        )
        .send()
        .await
        .expect("PUT /imposters");
    assert_eq!(response.status(), 400);
    let body = response.text().await.expect("body");
    assert!(body.contains("`repeat`"), "{body}");
    assert_eq!(
        imposter_count(&client, &base).await,
        before,
        "a refused wholesale replace must leave the running set untouched"
    );
    running.shutdown().await;
}

#[test]
fn the_datadir_reload_loader_refuses_an_unparseable_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = json!({ "port": 4545, "protocol": "http", "stubs": [string_bound_stub()] });
    std::fs::write(dir.path().join("4545.json"), config.to_string()).expect("write datadir file");

    let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
        .expect_err("an unparseable block in a datadir file must fail the load");
    let message = format!("{err:#}");
    assert!(message.contains("`wait`"), "{message}");
}

#[test]
fn a_configfile_with_an_unparseable_block_fails_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({
        "imposters": [{ "port": 4546, "protocol": "http", "stubs": [fractional_repeat_stub()] }]
    });
    std::fs::write(&path, config.to_string()).expect("write configfile");

    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect_err("an unparseable block in a configfile must fail the load");
    let message = format!("{err:#}");
    assert!(message.contains("`repeat`"), "{message}");
}

/// A bad value that a later `behaviors` array element overrides configures nothing: the engine
/// merges the array before parsing it, and rift-lint validates it the same way.
#[test]
fn a_configfile_whose_bad_value_is_overridden_still_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let config = json!({
        "imposters": [{ "port": 4547, "protocol": "http", "stubs": [{ "responses": [{
            "is": { "statusCode": 200 },
            "behaviors": [{ "wait": { "min": "1", "max": "2" } }, { "wait": 5 }]
        }] }] }]
    });
    std::fs::write(&path, config.to_string()).expect("write configfile");

    load_configs(&ConfigSource::File {
        path,
        no_parse: false,
    })
    .expect("an overridden value is not part of the block the engine parses");
}

#[tokio::test]
async fn reload_refuses_an_unparseable_block_and_leaves_the_running_set_unchanged() {
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
        "imposters": [{ "port": 0, "protocol": "http", "stubs": [fractional_repeat_stub()] }]
    });
    std::fs::write(&path, bad.to_string()).expect("rewrite configfile");

    let response = client
        .post(format!("{base}/admin/reload"))
        .send()
        .await
        .expect("POST /admin/reload");
    assert!(
        !response.status().is_success(),
        "a reload carrying an unparseable block must be refused, got {}",
        response.status()
    );
    assert_eq!(imposter_count(&client, &base).await, before);
    running.shutdown().await;
}
