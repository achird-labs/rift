//! Issue #269 gate: the static `is` response path must apply `${request.*}` templates,
//! `shellTransform`, and Prometheus request metrics — parity with the proxy path.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// AC1 — `${request.*}` expands on a static `is` body AND in response headers.
#[tokio::test]
async fn static_response_expands_request_templates() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19881, "protocol": "http", "stubs": [
                { "predicates": [{ "equals": { "path": "/tpl" } }],
                  "responses": [{ "is": { "statusCode": 200,
                    "headers": { "X-Echo-Path": "${request.path}" },
                    "body": "m=${request.method} p=${request.path} q=${request.query.x} h=${request.headers.x-foo}" } }] }
            ]
        }),
    )
    .await;

    let c = reqwest::Client::new();
    let resp = c
        .get("http://127.0.0.1:19881/tpl?x=1")
        .header("X-Foo", "bar")
        .send()
        .await
        .expect("send");
    let echo = resp
        .headers()
        .get("x-echo-path")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = resp.text().await.expect("body");
    assert_eq!(
        body, "m=GET p=/tpl q=1 h=bar",
        "request templates must expand on a static `is` body"
    );
    assert_eq!(
        echo.as_deref(),
        Some("/tpl"),
        "request templates must expand in response headers"
    );
    let _ = manager.delete_imposter(19881).await;
}

/// AC2 — `shellTransform` runs on a static `is` response.
#[tokio::test]
async fn static_response_applies_shell_transform() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19882, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "shellTransform": "printf transformed" } }] }
            ]
        }),
    )
    .await;

    let c = reqwest::Client::new();
    let body = c
        .get("http://127.0.0.1:19882/x")
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body");
    assert_eq!(
        body, "transformed",
        "shellTransform must run on a static `is` response"
    );
    let _ = manager.delete_imposter(19882).await;
}

/// AC3 — a non-proxied request increments `rift_requests_total` with the served status label.
/// Uses a unique status (418) so the counter is isolated from sibling `GET 200` tests AND the
/// recorded `status` label is verified to match the served status.
#[tokio::test]
async fn static_response_records_request_metric() {
    fn requests_total_418() -> u64 {
        rift_mock_core::extensions::collect_metrics()
            .lines()
            .filter(|l| l.starts_with("rift_requests_total") && l.contains("status=\"418\""))
            .filter_map(|l| l.rsplit(' ').next())
            .filter_map(|n| n.parse::<f64>().ok())
            .map(|f| f as u64)
            .sum()
    }

    let before = requests_total_418();
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19883, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 418, "body": "teapot" } }] }
            ]
        }),
    )
    .await;

    let c = reqwest::Client::new();
    let resp = c
        .get("http://127.0.0.1:19883/x")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 418, "served status");
    let _ = resp.text().await;

    let after = requests_total_418();
    assert_eq!(
        after,
        before + 1,
        "rift_requests_total{{status=418}} must increment by exactly one for the served request (before={before} after={after})"
    );
    let _ = manager.delete_imposter(19883).await;
}

/// AC2 (failure path) — a failing `shellTransform` leaves the body unchanged (warn-and-keep).
#[tokio::test]
async fn static_response_shell_transform_failure_keeps_body() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 19884, "protocol": "http", "stubs": [
                { "responses": [{ "is": { "statusCode": 200, "body": "original" },
                  "_behaviors": { "shellTransform": "exit 1" } }] }
            ]
        }),
    )
    .await;

    let c = reqwest::Client::new();
    let body = c
        .get("http://127.0.0.1:19884/x")
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body");
    assert_eq!(
        body, "original",
        "a failing shellTransform must leave the body unchanged"
    );
    let _ = manager.delete_imposter(19884).await;
}
