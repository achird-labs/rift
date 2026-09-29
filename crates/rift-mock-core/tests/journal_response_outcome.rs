//! Issue #1227: the `status` and `latencyMs` a journal entry carries (issue #364) describe the
//! answer the client actually received — nothing else. These tests drive the real serve loop
//! (`ImposterManager` + reqwest, like `journal_match_outcome.rs`) and assert the wire JSON of the
//! recorded entry.
//!
//! What is pinned here:
//!   - an ordinary answer records its status and a latency
//!   - an `X-Rift-Debug` request records neither: the report is about the request, not the stub's
//!     answer (hit and miss alike), and `matchOutcome` stays absent as it always was
//!   - a TCP fault records neither: the connection was aborted and no response was sent, so the
//!     `502` carrier the handler builds is not a status that went back
//!   - an engine error that IS answered records the status the client got
//!   - the two fields are always both present or both absent

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// One imposter, recording on, a single stub answering `path` with `response`.
async fn imposter_with(manager: &ImposterManager, path: &str, response: Value) -> u16 {
    create(
        manager,
        json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [{ "predicates": [{ "equals": { "path": path } }], "responses": [response] }]
        }),
    )
    .await
}

/// The port's single journal entry, as the JSON an operator reads from `savedRequests`.
fn only_entry(manager: &ImposterManager, port: u16) -> Value {
    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1, "exactly one request was recorded");
    serde_json::to_value(&recorded[0]).expect("serializes")
}

/// The #940 invariant, checked on every entry: both outcome fields or neither.
fn assert_outcome_fields_paired(entry: &Value) {
    assert_eq!(
        entry.get("status").is_some(),
        entry.get("latencyMs").is_some(),
        "status and latencyMs must be both present or both absent: {entry}"
    );
}

fn assert_no_response_outcome(entry: &Value) {
    assert_outcome_fields_paired(entry);
    assert!(entry.get("status").is_none(), "no status: {entry}");
    assert!(entry.get("latencyMs").is_none(), "no latencyMs: {entry}");
}

#[tokio::test]
async fn an_answered_request_records_its_status_and_latency() {
    let manager = ImposterManager::new();
    let port = imposter_with(
        &manager,
        "/a",
        json!({ "is": { "statusCode": 201, "body": "A" } }),
    )
    .await;
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/a"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 201);

    let entry = only_entry(&manager, port);
    assert_outcome_fields_paired(&entry);
    assert_eq!(entry["status"], 201, "{entry}");
    assert!(entry["latencyMs"].is_u64(), "{entry}");
    manager.delete_all().await;
}

async fn debug_get(port: u16, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("X-Rift-Debug", "true")
        .send()
        .await
        .expect("send")
}

#[tokio::test]
async fn a_debug_request_that_matches_records_no_response_outcome() {
    let manager = ImposterManager::new();
    let port = imposter_with(
        &manager,
        "/a",
        json!({ "is": { "statusCode": 201, "body": "A" } }),
    )
    .await;
    let resp = debug_get(port, "/a").await;
    assert_eq!(resp.status(), 200, "the debug report itself is a 200");
    assert!(resp.headers().contains_key("x-rift-debug-response"));

    let entry = only_entry(&manager, port);
    assert_no_response_outcome(&entry);
    assert!(entry.get("matchOutcome").is_none(), "{entry}");
    manager.delete_all().await;
}

#[tokio::test]
async fn a_debug_request_that_matches_nothing_records_no_response_outcome() {
    let manager = ImposterManager::new();
    let port = imposter_with(&manager, "/a", json!({ "is": { "body": "A" } })).await;
    let resp = debug_get(port, "/nomatch").await;
    assert_eq!(resp.status(), 200);

    let entry = only_entry(&manager, port);
    assert_eq!(entry["path"], "/nomatch");
    assert_no_response_outcome(&entry);
    assert!(entry.get("matchOutcome").is_none(), "{entry}");
    manager.delete_all().await;
}

/// A TCP fault aborts the connection, so the client's send fails; the journal still holds the
/// request and which stub matched it, but no status, because none was sent.
async fn assert_tcp_fault_records_no_response_outcome(response: Value) {
    let manager = ImposterManager::new();
    let port = imposter_with(&manager, "/fault", response).await;
    let sent = reqwest::get(format!("http://127.0.0.1:{port}/fault")).await;
    assert!(sent.is_err(), "the connection is aborted, got {sent:?}");

    let entry = only_entry(&manager, port);
    assert_no_response_outcome(&entry);
    assert!(entry.get("matchOutcome").is_some(), "matched: {entry}");
    manager.delete_all().await;
}

#[tokio::test]
async fn a_top_level_fault_records_no_response_outcome() {
    assert_tcp_fault_records_no_response_outcome(json!({ "fault": "CONNECTION_RESET_BY_PEER" }))
        .await;
}

#[tokio::test]
async fn a_rift_tcp_fault_records_no_response_outcome() {
    assert_tcp_fault_records_no_response_outcome(json!({
        "is": { "body": "never sent" },
        "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } }
    }))
    .await;
}

/// An engine error that the client does receive is an answer like any other: its status is
/// recorded. Only an outcome that was never sent is absent.
#[tokio::test]
async fn an_answered_engine_error_records_the_status_the_client_got() {
    let manager = ImposterManager::new();
    let port = imposter_with(
        &manager,
        "/boom",
        json!({ "inject": "function (config) { throw new Error('boom'); }" }),
    )
    .await;
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/boom"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);

    let entry = only_entry(&manager, port);
    assert_outcome_fields_paired(&entry);
    assert_eq!(entry["status"], 400, "{entry}");
    manager.delete_all().await;
}
