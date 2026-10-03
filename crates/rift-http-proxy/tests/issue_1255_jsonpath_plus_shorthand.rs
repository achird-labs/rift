//! Issue #1255: imposters written for Mountebank use jsonpath-plus shorthands RFC 9535 rejects —
//! `.[` (a descendant segment there) and a slice end of `0` (open-ended there). The whole imposter
//! was refused at creation. It now loads, and the selector selects what Mountebank selects.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn an_imposter_with_jsonpath_plus_shorthands_loads_and_matches() {
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

    let imposter = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{ "equals": { "body": "first" }, "jsonpath": { "selector": "$.x.y.[:0].z" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "SLICE" } }]
            },
            {
                "predicates": [{ "equals": { "body": "deep" }, "jsonpath": { "selector": "$.a.b.[0].c" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "DESCENDANT" } }]
            }
        ]
    });
    let created = client
        .post(format!("{base}/imposters"))
        .json(&imposter)
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
        .expect("assigned port");

    let send = |body: serde_json::Value| {
        let client = client.clone();
        async move {
            client
                .post(format!("http://127.0.0.1:{port}/"))
                .json(&body)
                .send()
                .await
                .expect("imposter request")
                .text()
                .await
                .expect("body")
        }
    };
    assert_eq!(
        send(json!({"x": {"y": [{"z": "first"}, {"z": "second"}]}})).await,
        "SLICE"
    );
    // `.[0]` descends: the match sits under an object-of-arrays that `$.a.b[0].c` cannot reach.
    assert_eq!(
        send(json!({"a": {"b": {"p": [{"c": "deep"}]}}})).await,
        "DESCENDANT"
    );
}
