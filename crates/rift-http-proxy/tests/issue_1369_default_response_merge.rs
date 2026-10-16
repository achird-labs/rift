//! Issue #1369: `defaultResponse` completes a stub's response, as in Mountebank.
//!
//! Mountebank's `postProcess` runs on every served response: a field the response does not
//! specify (`statusCode`, `headers`, `body`, `_mode`) is taken from `defaultResponse`. Rift read
//! `defaultResponse` only when no stub matched.
//!
//! Every imposter here takes an engine-assigned port, so none of these tests owns a fixed port.

use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

/// Create an imposter without a port; return the port the manager assigned.
async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("send")
}

fn stub(path: &str, response: Value) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }], "responses": [response] })
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

fn default_response() -> Value {
    json!({ "statusCode": 418, "headers": { "X-Default": "yes" }, "body": "dflt" })
}

#[tokio::test]
async fn default_response_completes_stub_responses() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": default_response(),
            "stubs": [
                stub("/empty", json!({ "is": {} })),
                stub("/status", json!({ "is": { "statusCode": 201 } })),
                stub("/hdr", json!({ "is": { "headers": { "A": "b" } } })),
                stub("/nohdr", json!({ "is": { "headers": {} } })),
                stub("/emptybody", json!({ "is": { "statusCode": 200, "body": "" } })),
                stub("/inject", json!({ "inject": "function(config) { return { body: 'i' }; }" })),
            ]
        }),
    )
    .await;

    let resp = get(port, "/empty").await;
    assert_eq!(resp.status(), 418);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    assert_eq!(resp.text().await.expect("body"), "dflt");

    let resp = get(port, "/status").await;
    assert_eq!(resp.status(), 201);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    assert_eq!(resp.text().await.expect("body"), "dflt");

    let resp = get(port, "/hdr").await;
    assert_eq!(resp.status(), 418);
    assert_eq!(header(&resp, "a"), Some("b"));
    assert_eq!(
        header(&resp, "x-default"),
        None,
        "headers are replaced whole, never merged key by key"
    );
    assert_eq!(resp.text().await.expect("body"), "dflt");

    let resp = get(port, "/nohdr").await;
    assert_eq!(resp.status(), 418);
    assert_eq!(
        header(&resp, "x-default"),
        None,
        "an explicit empty headers object is a specified headers value"
    );
    assert_eq!(resp.text().await.expect("body"), "dflt");

    // The documented deviation: Mountebank replaces a falsy `""`; Rift serves what was written.
    let resp = get(port, "/emptybody").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    assert_eq!(resp.text().await.expect("body"), "");

    let resp = get(port, "/inject").await;
    assert_eq!(resp.status(), 418);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    assert_eq!(resp.text().await.expect("body"), "i");

    let _ = manager.delete_imposter(port).await;
}

// `is: {}` has no templates or behaviors, so it is prepared at construction (#703). The
// prepared form knows nothing of the imposter's default, so the merge must still reach it.
#[tokio::test]
async fn default_response_merge_reaches_the_prepared_fast_path() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": default_response(),
            "stubs": [stub("/p", json!({ "is": {} }))]
        }),
    )
    .await;
    let resp = get(port, "/p").await;
    assert_eq!(resp.status(), 418);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    assert_eq!(resp.text().await.expect("body"), "dflt");
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn no_default_response_keeps_fast_path_and_old_answer() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "stubs": [stub("/p", json!({ "is": {} }))]
        }),
    )
    .await;
    let resp = get(port, "/p").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "");
    let _ = manager.delete_imposter(port).await;
}

// A proxy answer always has a status and headers, and its body is present even when empty, so
// the default never completes it.
#[tokio::test]
async fn proxy_response_is_not_completed_by_default_response() {
    let manager = ImposterManager::new();
    let upstream = create(
        &manager,
        json!({
            "protocol": "http",
            "stubs": [stub("/up", json!({ "is": { "statusCode": 200 } }))]
        }),
    )
    .await;
    let front = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": default_response(),
            "stubs": [stub("/up", json!({ "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"
            } }))]
        }),
    )
    .await;
    let resp = get(front, "/up").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-default"), None);
    assert_eq!(resp.text().await.expect("body"), "");
    let _ = manager.delete_imposter(front).await;
    let _ = manager.delete_imposter(upstream).await;
}

// `proxy` inside `defaultResponse` is not a feature (Mountebank does not proxy from it either),
// but it must not be erased from the printed config.
#[tokio::test]
async fn default_response_proxy_round_trips_and_warns() {
    let manager = Arc::new(ImposterManager::new());
    let server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None);
    let running = server.bind().await.expect("admin API binds");
    let admin = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "protocol": "http", "stubs": [],
            "defaultResponse": { "proxy": { "to": "http://127.0.0.1:1" } }
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(created.status().as_u16(), 201);
    let created: Value = created.json().await.expect("json");
    let port = u16::try_from(created["port"].as_u64().expect("port")).expect("u16");
    assert_eq!(
        created["defaultResponse"],
        json!({ "proxy": { "to": "http://127.0.0.1:1" } }),
        "{created}"
    );
    let warnings = created["_rift"]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w["warningType"] == "config_key_ignored"
                && w["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("defaultForward"))),
        "{created}"
    );

    let detail: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(
        detail["defaultResponse"],
        json!({ "proxy": { "to": "http://127.0.0.1:1" } })
    );

    // Not proxied: an unmatched request answers 200 with an empty body (port 1 is unreachable,
    // so any upstream contact would show as an error status).
    let resp = get(port, "/anything").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "");
    let _ = manager.delete_imposter(port).await;
}

// A 204 completed by a default with a body is served with no body and no content-length
// (the sdk-conformance corpus has `is: {statusCode: 204}` under a JSON default).
#[tokio::test]
async fn a_204_completed_by_a_default_body_is_served_without_one() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 404, "headers": { "Content-Type": "application/json" },
                                 "body": { "error": "not found" } },
            "stubs": [stub("/gone", json!({ "is": { "statusCode": 204 } }))]
        }),
    )
    .await;
    let resp = get(port, "/gone").await;
    assert_eq!(resp.status(), 204);
    assert_eq!(header(&resp, "content-type"), Some("application/json"));
    assert!(
        resp.headers().get("content-length").is_none(),
        "a 204 carries no content-length: {:?}",
        resp.headers()
    );
    assert_eq!(resp.bytes().await.expect("body").len(), 0);
    let _ = manager.delete_imposter(port).await;
}

// An inject's numeric-string status is its status, not a missing one.
#[tokio::test]
async fn an_inject_numeric_string_status_code_is_not_replaced_by_the_default() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": default_response(),
            "stubs": [stub("/s", json!({ "inject": "function(config) { return { statusCode: '201' }; }" }))]
        }),
    )
    .await;
    let resp = get(port, "/s").await;
    assert_eq!(resp.status(), 201);
    assert_eq!(header(&resp, "x-default"), Some("yes"));
    let _ = manager.delete_imposter(port).await;
}

// A recording of an empty upstream body wrote that body (`"body": ""`); the imposter's default
// body must not fill it on replay.
#[tokio::test]
async fn a_recorded_empty_upstream_body_is_not_filled_by_the_default_on_replay() {
    let manager = ImposterManager::new();
    let upstream = create(
        &manager,
        json!({
            "protocol": "http",
            "stubs": [stub("/up", json!({ "is": { "statusCode": 200 } }))]
        }),
    )
    .await;
    let front = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 404, "body": { "error": "x" } },
            "stubs": [{ "responses": [{ "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce",
                "predicateGenerators": [{ "matches": { "path": true } }]
            } }] }]
        }),
    )
    .await;
    let first = get(front, "/up").await;
    assert_eq!(first.status(), 200);
    // Take the upstream away: only the recorded stub can answer now.
    let _ = manager.delete_imposter(upstream).await;
    let replay = get(front, "/up").await;
    assert_eq!(
        replay.status(),
        200,
        "recorded status, not the default's 404"
    );
    assert_eq!(replay.text().await.expect("body"), "");
    let _ = manager.delete_imposter(front).await;
}
