//! Issue #1455: Mountebank's xmldom 0.9 reads past three non-fatal XML errors — a leading BOM,
//! text after the root element, and an entity XML does not define (kept as literal text). Rift's
//! strict parser refused the whole body, so an XPath predicate on such a body never matched.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;
use std::sync::Arc;

/// Status served for each body by an imposter whose only stub is `predicate`; the default
/// response (no stub matched) is a 200 with no body, so the stub answers 202 to be told apart.
async fn statuses(predicate: serde_json::Value, bodies: &[&str]) -> Vec<u16> {
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
    let stub = json!({ "predicates": [predicate], "responses": [{ "is": { "statusCode": 202 } }] });
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
        let status = client
            .post(format!("http://127.0.0.1:{port}/"))
            .body(body.to_string())
            .send()
            .await
            .expect("request to the imposter")
            .status()
            .as_u16();
        out.push(status);
    }
    running.shutdown().await;
    out
}

const BOM: &str = "\u{FEFF}<a><b>1</b></a>";
const NBSP: &str = "<a><b>x&nbsp;y</b></a>";

#[tokio::test]
async fn xpath_exists_matches_a_bom_body_and_an_undefined_entity_body() {
    let got = statuses(
        json!({ "exists": { "body": true }, "xpath": { "selector": "//b" } }),
        &[BOM, NBSP],
    )
    .await;
    assert_eq!(got, vec![202, 202]);
}

#[tokio::test]
async fn xpath_equals_reads_the_bom_body_and_the_entity_literally() {
    let bom = statuses(
        json!({ "equals": { "body": "1" }, "xpath": { "selector": "//b" } }),
        &[BOM],
    )
    .await;
    let nbsp = statuses(
        json!({ "equals": { "body": "x&nbsp;y" }, "xpath": { "selector": "//b" } }),
        &[NBSP],
    )
    .await;
    assert_eq!((bom, nbsp), (vec![202], vec![202]));
}

#[tokio::test]
async fn a_fatal_xml_error_still_does_not_match() {
    let got = statuses(
        json!({ "exists": { "body": true }, "xpath": { "selector": "//b" } }),
        &["<x:a><b>1</b></x:a>", "<a><b>1</b>"],
    )
    .await;
    assert_eq!(got, vec![200, 200]);
}
