//! Issue #1388: a scenario's state is a key in its flow, so a response that writes that key —
//! `_rift.stateOps`, or a script's `ctx.state` — is a scenario transition. When the scenario gates a
//! `_rift.conditional` stub, the changed answer must be served under a later `Last-Modified`, or a
//! client revalidating with `If-Modified-Since` is told `304` and keeps the stale one (#1307).
//!
//! The fixture: on `GET /a`, a conditional stub gated on `S1 == A` (body `q`) ahead of an ungated
//! conditional stub (body `p`); other paths write flow state from a response.

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::sync::LazyLock;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

const SESSION: &str = "s-1388";

fn conditional(body: &str) -> Value {
    json!({
        "is": { "statusCode": 200, "body": body },
        "_rift": { "conditional": { "etag": false, "lastModified": "load" } }
    })
}

fn writes(path: &str, rift: Value) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "is": { "statusCode": 200, "body": "ok" }, "_rift": rift }]
    })
}

fn script(path: &str, engine: &str, code: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "_rift": { "script": { "engine": engine, "code": code } } }]
    })
}

async fn imposter(manager: &ImposterManager) -> u16 {
    let set = |key: &str, value: &str| json!({ "stateOps": [{ "op": "set", "key": key, "value": value }] });
    let config = json!({
        "port": 0, "protocol": "http",
        "_rift": { "flowState": { "flowIdSource": "header:X-Session" } },
        "stubs": [
            {
                "scenarioName": "S1", "requiredScenarioState": "A",
                "predicates": [{ "equals": { "method": "GET", "path": "/a" } }],
                "responses": [conditional("q")]
            },
            {
                "predicates": [{ "equals": { "method": "GET", "path": "/a" } }],
                "responses": [conditional("p")]
            },
            writes("/set", set("S1", "A")),
            writes("/delete", json!({ "stateOps": [{ "op": "delete", "key": "S1" }] })),
            writes("/clear", json!({ "stateOps": [{ "op": "clearFlow" }] })),
            writes("/other", set("k1", "v")),
            script(
                "/rhai",
                "rhai",
                "fn respond(ctx) { ctx.state.set(\"S1\", \"A\"); http(200, \"ok\") }",
            ),
            script(
                "/js",
                "javascript",
                "function respond(ctx) { ctx.state.set(\"S1\", \"A\"); return http(200, \"ok\"); }",
            ),
        ]
    });
    manager
        .create_imposter(serde_json::from_value(config).expect("imposter config"))
        .await
        .expect("create")
}

async fn hit(port: u16, path: &str) {
    let response = CLIENT
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("X-Session", SESSION)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200, "{path}");
}

/// `GET /a`: the body and its `Last-Modified`, in seconds.
async fn get_a(port: u16) -> (String, i64) {
    let response = CLIENT
        .get(format!("http://127.0.0.1:{port}/a"))
        .header("X-Session", SESSION)
        .send()
        .await
        .expect("send");
    let lm = response
        .headers()
        .get("last-modified")
        .expect("a conditional answer carries Last-Modified")
        .to_str()
        .expect("ascii");
    let lm = chrono::DateTime::parse_from_rfc2822(lm)
        .expect("an HTTP date")
        .timestamp();
    (response.text().await.expect("body"), lm)
}

/// Moves `S1` to `A` through `path`, and checks the gated answer then carries a later stamp.
async fn entering_the_gate_moves_last_modified(path: &str) {
    let manager = ImposterManager::new();
    let port = imposter(&manager).await;

    let (before, t0) = get_a(port).await;
    assert_eq!(before, "p");
    hit(port, path).await;
    let (after, t1) = get_a(port).await;

    assert_eq!(after, "q", "{path} moved S1 into the gate");
    assert!(
        t1 > t0,
        "{path}: the answer changed under Last-Modified {t0} -> {t1}"
    );
    manager.delete_all().await;
}

/// Puts `S1` in `A`, then leaves it through `path`: the ungated answer must carry a later stamp.
async fn leaving_the_gate_moves_last_modified(path: &str) {
    let manager = ImposterManager::new();
    let port = imposter(&manager).await;

    hit(port, "/set").await;
    let (before, t0) = get_a(port).await;
    assert_eq!(before, "q");
    hit(port, path).await;
    let (after, t1) = get_a(port).await;

    assert_eq!(after, "p", "{path} returned S1 to Started");
    assert!(
        t1 > t0,
        "{path}: the answer changed under Last-Modified {t0} -> {t1}"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn a_state_ops_set_of_a_scenario_key_moves_last_modified() {
    entering_the_gate_moves_last_modified("/set").await;
}

#[tokio::test]
async fn a_state_ops_delete_of_a_scenario_key_moves_last_modified() {
    leaving_the_gate_moves_last_modified("/delete").await;
}

#[tokio::test]
async fn a_state_ops_clear_flow_moves_last_modified() {
    leaving_the_gate_moves_last_modified("/clear").await;
}

#[tokio::test]
async fn a_rhai_ctx_state_write_to_a_scenario_key_moves_last_modified() {
    entering_the_gate_moves_last_modified("/rhai").await;
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn a_js_ctx_state_write_to_a_scenario_key_moves_last_modified() {
    entering_the_gate_moves_last_modified("/js").await;
}

// Writing the state the scenario already has is not a transition: the stamp stays put.
#[tokio::test]
async fn writing_the_state_a_scenario_already_has_keeps_last_modified() {
    let manager = ImposterManager::new();
    let port = imposter(&manager).await;

    hit(port, "/set").await;
    let (_, t0) = get_a(port).await;
    hit(port, "/set").await;
    let (body, t1) = get_a(port).await;

    assert_eq!(body, "q");
    assert_eq!(t1, t0, "an unchanged scenario state moved Last-Modified");
    manager.delete_all().await;
}

// A key that gates nothing is not a scenario: writing it never moves the stamp.
#[tokio::test]
async fn writing_a_key_that_gates_nothing_keeps_last_modified() {
    let manager = ImposterManager::new();
    let port = imposter(&manager).await;

    let (_, t0) = get_a(port).await;
    hit(port, "/other").await;
    let (body, t1) = get_a(port).await;

    assert_eq!(body, "p");
    assert_eq!(t1, t0);
    manager.delete_all().await;
}
