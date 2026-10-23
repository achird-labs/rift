//! Issue #1635: the `statusCode` an `inject` returns, and the one a behavior leaves on a `proxy`
//! response, reach the response the way Mountebank's do. Mountebank takes the inject's return value
//! as the response unvalidated (`responseResolver.js`), `postProcess` falls back past a falsy
//! status only (`baseHttpServer.js:25`), and Node's `writeHead` reads the rest as
//! `Number(value) | 0`, answering `500` `ERR_HTTP_INVALID_STATUS_CODE` outside 100–999
//! (`:143-155`). A proxy runs its behaviors on the upstream response and records and serves what
//! they leave (`proxyAndRecord`).
#![cfg(feature = "javascript")]

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

/// Creates the imposter on an auto-assigned port (issue #1438) and returns it.
async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config decodes");
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

/// Asserts the `500` Mountebank answers for a status Node refuses, naming the value.
async fn assert_invalid_status(port: u16, path: &str, value: &str) {
    let resp = get(port, path).await;
    assert_eq!(resp.status(), 500, "{path}: an invalid status answers 500");
    let body: Value = resp.json().await.expect("json body");
    let error = &body["errors"][0];
    assert_eq!(
        error["code"], "ERR_HTTP_INVALID_STATUS_CODE",
        "{path}: {body}"
    );
    assert_eq!(error["name"], "RangeError", "{path}: {body}");
    assert_eq!(
        error["message"],
        format!("Invalid status code: {value}"),
        "{path}: {body}"
    );
}

fn inject_stub(path: &str, status: &str) -> Value {
    json!({
        "predicates": [ { "equals": { "path": path } } ],
        "responses": [ { "inject": format!(
            "function (config) {{ return {{ statusCode: {status}, body: 'injected' }}; }}"
        ) } ]
    })
}

#[tokio::test]
async fn an_inject_string_status_node_refuses_answers_500() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            inject_stub("/word", "'abc'"),
            inject_stub("/true", "true"),
            inject_stub("/big", "70000"),
        ] }),
    )
    .await;

    assert_invalid_status(port, "/word", "abc").await;
    assert_invalid_status(port, "/true", "true").await;
    // Not saturated to 65535: Node names the value as written.
    assert_invalid_status(port, "/big", "70000").await;

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn an_inject_negative_status_answers_500_not_the_default() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 418 },
            "stubs": [
                inject_stub("/negative", "-1"),
                // Falsy values do take the default, as `postProcess` does.
                inject_stub("/zero", "0"),
                inject_stub("/nan", "NaN"),
                inject_stub("/empty", "''"),
            ]
        }),
    )
    .await;

    assert_invalid_status(port, "/negative", "-1").await;
    assert_eq!(get(port, "/zero").await.status(), 418);
    assert_eq!(get(port, "/nan").await.status(), 418);
    assert_eq!(get(port, "/empty").await.status(), 418);

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn an_inject_numeric_string_with_whitespace_is_served() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [
            inject_stub("/padded", "' 404 '"),
            inject_stub("/fraction", "404.7"),
            inject_stub("/numeric", "'201'"),
        ] }),
    )
    .await;

    let resp = get(port, "/padded").await;
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.expect("body"), "injected");
    assert_eq!(get(port, "/fraction").await.status(), 404);
    assert_eq!(get(port, "/numeric").await.status(), 201);

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn an_inject_numeric_header_value_is_sent() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ { "inject":
            "function () { return { headers: { 'X-Count': 5, 'X-Flag': false }, body: 'h' }; }"
        } ] } ] }),
    )
    .await;

    let resp = get(port, "/").await;
    assert_eq!(resp.status(), 200);
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    assert_eq!(header("x-count").as_deref(), Some("5"));
    assert_eq!(header("x-flag").as_deref(), Some("false"));

    let _ = manager.delete_imposter(port).await;
}

/// Mountebank serves an object body with `JSON.stringify`, which throws for a cycle, and answers
/// `500`. Rift answers its inject-error `500`, rather than overflowing the stack.
#[tokio::test]
async fn an_inject_cyclic_body_answers_500() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ { "inject":
            "function () { const o = {}; o.self = o; return { body: o }; }"
        } ] } ] }),
    )
    .await;

    let resp = get(port, "/").await;
    assert_eq!(resp.status(), 500);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["errors"][0]["code"], "invalid injection", "{body}");

    let _ = manager.delete_imposter(port).await;
}

/// A body nested far past the cap answers 500 instead of overflowing the stack.
#[tokio::test]
async fn an_inject_deeply_nested_body_answers_500() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ { "inject":
            "function () { let o = {}; for (let i = 0; i < 100000; i++) { o = { a: o }; } return { body: o }; }"
        } ] } ] }),
    )
    .await;

    let resp = get(port, "/").await;
    assert_eq!(resp.status(), 500);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["errors"][0]["code"], "invalid injection", "{body}");

    let _ = manager.delete_imposter(port).await;
}

/// An upstream answering 200, and a recording `proxyOnce` imposter in front of it whose `decorate`
/// sets `statusCode` to `status`.
async fn decorated_proxy(manager: &ImposterManager, status: &str) -> (u16, u16) {
    let upstream = create(
        manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ { "is": { "body": "upstream" } } ] } ] }),
    )
    .await;
    let proxy = create(
        manager,
        json!({ "protocol": "http", "stubs": [ { "responses": [ {
            // Generators make the proxy record a stub, which `GET /imposters/:port` shows.
            "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"),
                "mode": "proxyOnce",
                "predicateGenerators": [ { "matches": { "path": true } } ]
            },
            "behaviors": [ { "decorate": format!(
                "function (request, response) {{ response.statusCode = {status}; }}"
            ) } ]
        } ] } ] }),
    )
    .await;
    (upstream, proxy)
}

/// The recorded stubs: every stub but the proxy stub itself, which stays last.
fn recorded_stubs(manager: &ImposterManager, port: u16) -> Vec<Value> {
    let stubs = manager
        .get_imposter(port)
        .expect("imposter exists")
        .get_stubs();
    let mut stubs: Vec<Value> = stubs
        .iter()
        .map(|stub| serde_json::to_value(stub).expect("stub serializes"))
        .collect();
    stubs.pop();
    stubs
}

#[tokio::test]
async fn a_decorate_string_status_on_a_proxy_is_served() {
    let manager = ImposterManager::new();
    let (upstream, proxy) = decorated_proxy(&manager, "'404'").await;

    let resp = get(proxy, "/p").await;
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.expect("body"), "upstream");

    let recorded = recorded_stubs(&manager, proxy);
    assert_eq!(recorded.len(), 1, "one stub recorded: {recorded:?}");
    assert_eq!(
        recorded[0]["responses"][0]["is"]["statusCode"],
        json!(404),
        "a numeric string records as the number: {}",
        recorded[0]
    );
    // The replay serves the recorded status.
    assert_eq!(get(proxy, "/p").await.status(), 404);

    let _ = manager.delete_imposter(proxy).await;
    let _ = manager.delete_imposter(upstream).await;
}

#[tokio::test]
async fn a_decorate_status_node_refuses_on_a_proxy_answers_500_and_records_nothing() {
    let manager = ImposterManager::new();
    let (upstream, proxy) = decorated_proxy(&manager, "'abc'").await;

    assert_invalid_status(proxy, "/p", "abc").await;
    assert!(
        recorded_stubs(&manager, proxy).is_empty(),
        "a refused status records nothing"
    );
    // Nothing was recorded, so the next request proxies again and is refused again.
    assert_invalid_status(proxy, "/p", "abc").await;

    let _ = manager.delete_imposter(proxy).await;
    let _ = manager.delete_imposter(upstream).await;
}
