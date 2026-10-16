//! Issue #1486: with an `xpath` or `jsonpath` selector Rift applies `except` to each selected
//! value, where Mountebank applies it to the whole body before selecting. The two bodies below stop
//! matching in Mountebank (differences.md, "`except` with an `xpath` or `jsonpath` selector") and
//! must keep matching in Rift. `caseSensitive` is explicit so the order is the only variable.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

/// Status and body served for each request body by an imposter whose only stub is `stub`; the
/// default response (no stub matched) is a 200 with no body, so the stubs answer 202.
async fn served(stub: serde_json::Value, bodies: &[&str]) -> Vec<(u16, String)> {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    let base = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&json!({ "port": 0, "protocol": "http", "stubs": [stub] }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        created.status(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );
    let port = created.json::<serde_json::Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");
    let mut out = Vec::new();
    for body in bodies {
        let response = client
            .post(format!("http://127.0.0.1:{port}/"))
            .body(body.to_string())
            .send()
            .await
            .expect("request to the imposter");
        let status = response.status().as_u16();
        out.push((status, response.text().await.expect("body")));
    }
    running.shutdown().await;
    out
}

async fn statuses(predicate: serde_json::Value, bodies: &[&str]) -> Vec<u16> {
    let stub = json!({ "predicates": [predicate], "responses": [{ "is": { "statusCode": 202 } }] });
    served(stub, bodies)
        .await
        .into_iter()
        .map(|(status, _)| status)
        .collect()
}

#[tokio::test]
async fn an_xpath_except_that_removes_markup_still_matches() {
    let got = statuses(
        json!({ "equals": { "body": "1" }, "caseSensitive": true, "except": "<a>",
                "xpath": { "selector": "//b" } }),
        &["<a><b>1</b></a>"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

#[tokio::test]
async fn a_jsonpath_except_that_removes_quotes_still_matches() {
    let got = statuses(
        json!({ "equals": { "body": "1" }, "caseSensitive": true, "except": "\"",
                "jsonpath": { "selector": "$.a" } }),
        &["{\"a\":1}"],
    )
    .await;
    assert_eq!(got, vec![202]);
}

#[tokio::test]
async fn except_strips_digits_from_each_selected_value() {
    let xpath = statuses(
        json!({ "equals": { "body": "x" }, "caseSensitive": true, "except": "\\d+",
                "xpath": { "selector": "//b" } }),
        &["<a><b>x1</b></a>", "<a><b>y1</b></a>"],
    )
    .await;
    let jsonpath = statuses(
        json!({ "equals": { "body": "x" }, "caseSensitive": true, "except": "\\d+",
                "jsonpath": { "selector": "$.a" } }),
        &["{\"a\":\"x1\"}", "{\"a\":\"y1\"}"],
    )
    .await;
    assert_eq!((xpath, jsonpath), (vec![202, 200], vec![202, 200]));
}
