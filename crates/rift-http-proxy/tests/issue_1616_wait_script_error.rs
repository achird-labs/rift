//! Issue #1616 (part A): a `wait` function that throws answers Mountebank's `500 invalid wait
//! injection`, not the stub's response after a 100 ms fallback.
//!
//! Mountebank 2.9.5 (`src/models/behaviors.js` `wait`) evaluates `(<wait>)()` inside a `try`, and
//! a throw becomes `InjectionError('invalid wait injection', { source, data: error.message })`,
//! which the HTTP server answers with `500` and `code: "invalid injection"`. It has no lenient mode
//! for this, so `strictBehaviors` does not change the answer. A result that is not a number is not
//! an error: it is no delay (issue #1588).
//!
//! Every imposter here takes an engine-assigned port, so none of these tests owns a fixed port.
#![cfg(feature = "javascript")]

use std::time::Duration;

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create")
}

/// The status, headers and body of `GET path`, bounded so a regression to a long wait fails fast.
async fn get(port: u16, path: &str) -> (u16, reqwest::header::HeaderMap, String) {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("send");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.expect("body"))
}

/// A stub on `path` answering `200 ok` after `wait`.
fn is_stub(path: &str, wait: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{
            "is": { "statusCode": 200, "body": "ok" },
            "_behaviors": { "wait": wait }
        }]
    })
}

const THROWS: &str = "function(){ throw new Error('x'); }";
const REFERENCE_ERROR: &str = "function(){ return nosuch.x; }";
const SYNTAX_ERROR: &str = "function(){ return ; }}";

/// Assert `GET path` is Mountebank's wait-injection failure, and return its one error object.
async fn assert_wait_error(port: u16, path: &str) -> Value {
    let (status, headers, body) = get(port, path).await;
    assert_eq!(status, 500, "{path}: a throwing wait is a 500, got {body}");
    assert_eq!(
        headers
            .get("x-rift-wait-error")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "{path}: the failure is marked as a wait failure"
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "{path}"
    );
    assert!(
        !body.contains("\"ok\"") && body != "ok",
        "{path}: the stub's own response must not be served, got {body}"
    );
    let json: Value = serde_json::from_str(&body).expect("error envelope is JSON");
    let error = json["errors"][0].clone();
    assert_eq!(error["code"], "invalid injection", "{path}: {body}");
    assert_eq!(error["type"], "invalid injection", "{path}: {body}");
    assert_eq!(error["message"], "invalid wait injection", "{path}: {body}");
    error
}

#[tokio::test]
async fn a_throwing_wait_function_answers_500_invalid_wait_injection() {
    for strict in [false, true] {
        let manager = ImposterManager::new();
        let port = create(
            &manager,
            json!({ "protocol": "http", "strictBehaviors": strict, "stubs": [
                is_stub("/throw", THROWS),
                is_stub("/reference", REFERENCE_ERROR),
                is_stub("/syntax", SYNTAX_ERROR),
                is_stub("/string", "function(){ throw 'plain'; }"),
            ]}),
        )
        .await;

        let thrown = assert_wait_error(port, "/throw").await;
        assert_eq!(thrown["data"], "x", "strict={strict}: data is the message");

        let reference = assert_wait_error(port, "/reference").await;
        assert_eq!(
            reference["data"], "nosuch is not defined",
            "strict={strict}: a ReferenceError's message"
        );

        // The create succeeds and the error is found at serve time, as in Mountebank. Boa words a
        // syntax error differently from V8, so only its presence is pinned.
        let syntax = assert_wait_error(port, "/syntax").await;
        assert!(
            syntax["data"].as_str().is_some_and(|d| !d.is_empty()),
            "strict={strict}: a syntax error carries its message, got {syntax}"
        );

        // `throw 'plain'` has no `message`, so Mountebank's `data` is undefined and left out.
        let plain = assert_wait_error(port, "/string").await;
        assert!(
            plain.get("data").is_none(),
            "strict={strict}: a thrown non-Error has no data, got {plain}"
        );
    }
}

// Issue #1588 already made a non-numeric result no delay; only a throw is an error.
#[tokio::test]
async fn a_non_numeric_wait_result_is_still_served() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            is_stub("/nan", "function(){ return 'abc'; }"),
            is_stub("/undefined", "function(){ }"),
        ]}),
    )
    .await;
    for path in ["/nan", "/undefined"] {
        let (status, headers, body) = get(port, path).await;
        assert_eq!((status, body.as_str()), (200, "ok"), "{path}");
        assert!(!headers.contains_key("x-rift-wait-error"), "{path}");
    }
}

// The same failure on the other two response types that run behaviors.
#[tokio::test]
async fn a_throwing_wait_fails_inject_and_proxy_responses_too() {
    let manager = ImposterManager::new();
    let upstream = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            { "responses": [{ "is": { "statusCode": 200, "body": "upstream" } }] }
        ]}),
    )
    .await;
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            {
                "predicates": [{ "equals": { "path": "/inject" } }],
                "responses": [{
                    "inject": "function() { return { statusCode: 200, body: 'ok' }; }",
                    "_behaviors": { "wait": THROWS }
                }]
            },
            {
                "predicates": [{ "equals": { "path": "/proxy" } }],
                "responses": [{
                    "proxy": { "to": format!("http://127.0.0.1:{upstream}") },
                    "_behaviors": { "wait": THROWS }
                }]
            }
        ]}),
    )
    .await;
    for path in ["/inject", "/proxy"] {
        let error = assert_wait_error(port, path).await;
        assert_eq!(error["data"], "x", "{path}");
    }
}
