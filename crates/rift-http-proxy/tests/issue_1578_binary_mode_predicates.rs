//! Issue #1578: an imposter created with `"mode": "binary"` refuses `matches` predicates and
//! `jsonpath`/`xpath` selectors with 400, as Mountebank's creation dry run does. Rift accepted
//! them with 201.
//!
//! `mode` is creation-time only in Mountebank: it is never printed, the stub routes validate
//! against the stored imposter (which has no `mode`) and so accept the same stub, and
//! `--configfile` loads through `PUT /imposters`. These tests drive each of those doors.

mod support;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::sync::Arc;

const MATCHES: &str = "the matches predicate is not allowed in binary mode";
const JSONPATH: &str = "the jsonpath predicate parameter is not allowed in binary mode";
const XPATH: &str = "the xpath predicate parameter is not allowed in binary mode";

fn stub_with(predicate: Value) -> Value {
    json!({
        "predicates": [predicate],
        "responses": [{ "is": { "statusCode": 200, "body": "MATCHED" } }]
    })
}

fn binary(protocol: &str, stubs: Vec<Value>) -> Value {
    json!({ "protocol": protocol, "mode": "binary", "stubs": stubs })
}

/// Every refused shape from the triage probe, with the message Mountebank 2.9.5 answers.
fn refused() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "matches",
            binary(
                "http",
                vec![stub_with(json!({ "matches": { "body": "^AQ" } }))],
            ),
            MATCHES,
        ),
        (
            "equals + jsonpath",
            binary(
                "http",
                vec![stub_with(json!({
                    "equals": { "body": "x" }, "jsonpath": { "selector": "$.a" }
                }))],
            ),
            JSONPATH,
        ),
        (
            "equals + xpath",
            binary(
                "http",
                vec![stub_with(json!({
                    "equals": { "body": "x" }, "xpath": { "selector": "//a" }
                }))],
            ),
            XPATH,
        ),
        (
            "exists + jsonpath",
            binary(
                "http",
                vec![stub_with(json!({
                    "exists": { "body": true }, "jsonpath": { "selector": "$.a" }
                }))],
            ),
            JSONPATH,
        ),
        (
            "not matches",
            binary(
                "http",
                vec![stub_with(json!({ "not": { "matches": { "body": "x" } } }))],
            ),
            MATCHES,
        ),
        (
            "or [false, matches]",
            binary(
                "http",
                vec![stub_with(json!({ "or": [
                    { "equals": { "path": "/zzz" } },
                    { "matches": { "body": "x" } }
                ] }))],
            ),
            MATCHES,
        ),
        (
            "https matches",
            binary(
                "https",
                vec![stub_with(json!({ "matches": { "body": "x" } }))],
            ),
            MATCHES,
        ),
        (
            "second of two stubs",
            binary(
                "http",
                vec![
                    stub_with(json!({ "equals": { "body": "x" } })),
                    stub_with(json!({ "matches": { "body": "x" } })),
                ],
            ),
            MATCHES,
        ),
        // A deliberate difference (docs/mountebank/differences.md): Mountebank's dry run
        // short-circuits these two on its synthetic request and admits them; Rift refuses a
        // `matches` anywhere in a binary imposter.
        (
            "or [true, matches]",
            binary(
                "http",
                vec![stub_with(json!({ "or": [
                    { "equals": { "path": "/" } },
                    { "matches": { "body": "x" } }
                ] }))],
            ),
            MATCHES,
        ),
        (
            "and [false, matches]",
            binary(
                "http",
                vec![stub_with(json!({ "and": [
                    { "equals": { "path": "/nomatch" } },
                    { "matches": { "body": "x" } }
                ] }))],
            ),
            MATCHES,
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

async fn get_json(client: &reqwest::Client, url: String) -> Value {
    client
        .get(url)
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("JSON")
}

async fn imposter_count(client: &reqwest::Client, base: &str) -> usize {
    get_json(client, format!("{base}/imposters")).await["imposters"]
        .as_array()
        .expect("imposters is an array")
        .len()
}

async fn assert_bad_data(response: reqwest::Response, message: &str, door: &str) {
    assert_eq!(response.status(), 400, "{door} must refuse");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("bad data"),
        "{door}: bad data envelope: {body}"
    );
    assert!(body.contains(message), "{door}: names `{message}`: {body}");
}

#[tokio::test]
async fn post_imposters_refuses_binary_mode_selectors() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    for (case, imposter, message) in refused() {
        let response = client
            .post(format!("{base}/imposters"))
            .json(&imposter)
            .send()
            .await
            .expect("POST /imposters");
        assert_bad_data(response, message, &format!("POST /imposters ({case})")).await;
        assert_eq!(
            imposter_count(&client, &base).await,
            0,
            "{case}: a refused imposter must not be registered"
        );
    }
    running.shutdown().await;
}

#[tokio::test]
async fn put_imposters_refuses_binary_mode_selectors_and_changes_nothing() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);

    for (case, imposter, message) in refused() {
        let response = client
            .put(format!("{base}/imposters"))
            .json(&json!({ "imposters": [imposter] }))
            .send()
            .await
            .expect("PUT /imposters");
        assert_bad_data(response, message, &format!("PUT /imposters ({case})")).await;
        assert_eq!(
            imposter_count(&client, &base).await,
            1,
            "{case}: a refused replace must leave the running set untouched"
        );
    }
    running.shutdown().await;
}

/// The check is keyed on exactly `"binary"`, as Mountebank's `request.mode === 'binary'` is.
#[tokio::test]
async fn matches_is_admitted_without_binary_mode() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let stub = stub_with(json!({ "matches": { "body": "x" } }));
    for mode in [None, Some("text"), Some("Binary")] {
        let mut imposter = json!({ "port": 0, "protocol": "http", "stubs": [stub.clone()] });
        if let Some(mode) = mode {
            imposter["mode"] = mode.into();
        }
        let response = client
            .post(format!("{base}/imposters"))
            .json(&imposter)
            .send()
            .await
            .expect("POST /imposters");
        assert_eq!(response.status(), 201, "mode {mode:?}");
    }
    running.shutdown().await;
}

/// A binary imposter with no selector is created; `mode` is never printed, and a later stub edit is
/// not held to it, as Mountebank's stub routes validate against the stored imposter.
#[tokio::test]
async fn a_binary_imposter_forgets_mode_after_creation() {
    let (running, base) = admin().await;
    let client = reqwest::Client::new();
    let mut imposter = binary(
        "http",
        vec![stub_with(json!({ "equals": { "body": "AQID" } }))],
    );
    imposter["port"] = 0.into();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.expect("json");
    assert!(created.get("mode").is_none(), "201 body: {created}");
    let port = created["port"].as_u64().expect("port");

    for url in [
        format!("{base}/imposters/{port}"),
        format!("{base}/imposters/{port}?replayable=true"),
        format!("{base}/imposters?replayable=true"),
    ] {
        let printed = get_json(&client, url.clone()).await;
        assert!(
            !printed.to_string().contains("\"mode\""),
            "{url} prints no mode: {printed}"
        );
    }

    let stub = stub_with(json!({ "matches": { "body": "x" } }));
    let response = client
        .post(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stub": stub.clone() }))
        .send()
        .await
        .expect("POST stubs");
    assert_eq!(response.status(), 200, "POST /imposters/:port/stubs");
    let response = client
        .put(format!("{base}/imposters/{port}/stubs"))
        .json(&json!({ "stubs": [stub.clone()] }))
        .send()
        .await
        .expect("PUT stubs");
    assert_eq!(response.status(), 200, "PUT /imposters/:port/stubs");
    let response = client
        .put(format!("{base}/imposters/{port}/stubs/0"))
        .json(&stub)
        .send()
        .await
        .expect("PUT stub by index");
    assert_eq!(response.status(), 200, "PUT /imposters/:port/stubs/0");
    running.shutdown().await;
}

/// `--configfile` creates through the same door, so the imposter is refused and the startup log
/// names Mountebank's message.
#[tokio::test]
async fn a_configfile_binary_imposter_with_matches_is_refused_at_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let configfile = dir.path().join("imposters.json");
    let mut imposter = binary(
        "http",
        vec![stub_with(json!({ "matches": { "body": "x" } }))],
    );
    imposter["port"] = 0.into();
    std::fs::write(&configfile, json!({ "imposters": [imposter] }).to_string())
        .expect("write configfile");
    let log = dir.path().join("rift.log");
    let file = std::fs::File::create(&log).expect("create log");
    let mut child = tokio::process::Command::new(support::server_bin())
        .args(["--port", "0", "--host", "127.0.0.1", "--configfile"])
        .arg(&configfile)
        .env("NO_COLOR", "1")
        .stdout(file.try_clone().expect("clone log"))
        .stderr(file)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rift server");

    let mut admin = None;
    for _ in 0..100 {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        admin = text
            .lines()
            .filter_map(|l| l.split("listening on http://").nth(1))
            .filter_map(|rest| {
                rest.split_whitespace()
                    .next()?
                    .rsplit(':')
                    .next()?
                    .parse::<u16>()
                    .ok()
            })
            .next();
        if admin.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut text = std::fs::read_to_string(&log).unwrap_or_default();
    let admin = admin.unwrap_or_else(|| panic!("no admin port within 10s; output:\n{text}"));
    for _ in 0..50 {
        if text.contains(MATCHES) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        text = std::fs::read_to_string(&log).unwrap_or_default();
    }
    assert!(
        text.contains(MATCHES),
        "the startup log names the refusal:\n{text}"
    );
    let client = reqwest::Client::new();
    assert_eq!(
        imposter_count(&client, &format!("http://127.0.0.1:{admin}")).await,
        0,
        "the refused imposter is not served"
    );
    child.kill().await.expect("kill rift");
}
