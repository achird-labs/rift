//! Issue #1567: a `decorate` runs on Mountebank's contract, whatever function form it is written in.
//!
//! Mountebank calls every decorate as `(fn)(config, response, logger)`
//! (`src/models/behaviors.js:239`). Any function form works, whatever its parameters are named. A
//! truthy return value replaces the response, and the server then completes it from
//! `defaultResponse`. A falsy one keeps the response the function changed. Rift sent every arrow
//! not named `config` to Rhai, ignored what the function returned, and merged the script's headers
//! back over the originals, so a deleted header came back.
//!
//! Every imposter here takes an engine-assigned port, so none of these tests owns a fixed port.
#![cfg(feature = "javascript")]

use std::time::Duration;

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// The status, the named headers, and the body of `GET path`.
async fn get(port: u16, path: &str) -> (u16, reqwest::header::HeaderMap, String) {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.expect("body"))
}

/// A stub on `path` answering `statusCode` with `X-Orig: 1` and `orig`, decorated by `script`.
fn stub(path: &str, status: u16, script: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{
            "is": { "statusCode": status, "headers": { "X-Orig": "1" }, "body": "orig" },
            "behaviors": [{ "decorate": script }]
        }]
    })
}

fn has(headers: &reqwest::header::HeaderMap, name: &str) -> bool {
    headers.contains_key(name)
}

#[tokio::test]
async fn every_function_form_is_applied_as_javascript() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            stub("/two", 200, "(request, response) => { response.body = 'mutated ' + request.path; }"),
            stub("/short", 200, "(req, res) => { res.body = 'a'; }"),
            stub("/single", 200, "request => { request.response.body = 'si'; }"),
            stub("/async", 200, "async (request, response) => { response.body = 'as'; }"),
            stub("/fn", 200, "function (request, response) { response.body = 'fn'; }"),
            stub("/cfg", 200, "config => { config.response.body = 'cfg'; }"),
        ]}),
    )
    .await;

    for (path, want) in [
        ("/two", "mutated /two"),
        ("/short", "a"),
        ("/single", "si"),
        ("/async", "as"),
        ("/fn", "fn"),
        ("/cfg", "cfg"),
    ] {
        let (status, headers, body) = get(port, path).await;
        assert!(
            !has(&headers, "x-rift-decorate-error"),
            "{path}: the decorate must run, not fail"
        );
        assert_eq!((status, body.as_str()), (200, want), "{path}");
        assert!(
            has(&headers, "x-orig"),
            "{path}: a mutation keeps the headers"
        );
    }
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_returned_object_replaces_the_response() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            stub("/ret", 404, "function (request, response) { return { body: 'returned' }; }"),
            stub("/status", 200, "config => ({ statusCode: 201, body: 'cfgret' })"),
            stub("/hdrs", 404, "config => ({ headers: { 'X-New': 'n' }, body: 'b' })"),
            stub("/falsy", 200, "config => { config.response.body = 'mut'; return 0; }"),
        ]}),
    )
    .await;

    let (status, headers, body) = get(port, "/ret").await;
    assert_eq!((status, body.as_str()), (200, "returned"));
    assert!(
        !has(&headers, "x-orig"),
        "a returned object replaces the headers"
    );

    let (status, _, body) = get(port, "/status").await;
    assert_eq!((status, body.as_str()), (201, "cfgret"));

    let (status, headers, body) = get(port, "/hdrs").await;
    assert_eq!((status, body.as_str()), (200, "b"));
    assert!(has(&headers, "x-new"));
    assert!(!has(&headers, "x-orig"));

    let (status, headers, body) = get(port, "/falsy").await;
    assert_eq!(
        (status, body.as_str()),
        (200, "mut"),
        "a falsy return keeps the mutated response"
    );
    assert!(has(&headers, "x-orig"));
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_returned_object_is_completed_from_the_default_response() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 418, "headers": { "X-Def": "d" } },
            "stubs": [stub("/ret", 404, "config => ({ body: 'ret' })")]
        }),
    )
    .await;
    let (status, headers, body) = get(port, "/ret").await;
    assert_eq!((status, body.as_str()), (418, "ret"));
    assert!(has(&headers, "x-def"));
    assert!(!has(&headers, "x-orig"));
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_deleted_header_is_not_sent() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            stub("/cfg", 200, "config => { delete config.response.headers['X-Orig']; config.response.body = 'd'; }"),
            stub("/fn", 200, "function (request, response) { delete response.headers['X-Orig']; response.body = 'd'; }"),
        ]}),
    )
    .await;
    for path in ["/cfg", "/fn"] {
        let (status, headers, body) = get(port, path).await;
        assert_eq!((status, body.as_str()), (200, "d"), "{path}");
        assert!(
            !has(&headers, "x-orig"),
            "{path}: the deleted header came back"
        );
    }
    let _ = manager.delete_imposter(port).await;
}
