//! Issue #295 gate: imposter listeners auto-negotiate HTTP/1 and HTTP/2, so an HTTP/2 (h2c
//! prior-knowledge) client can talk to Rift while HTTP/1 clients keep working unchanged.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

async fn mk(manager: &ImposterManager, port: u16) {
    let cfg = serde_json::json!({
        "port": port, "protocol": "http", "stubs": [
            { "responses": [{ "is": { "statusCode": 200, "body": "h2-ok" } }] }
        ]
    });
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

// AC1: an HTTP/2 prior-knowledge (h2c) client gets a served response over HTTP/2.
#[tokio::test]
async fn imposter_serves_http2_prior_knowledge() {
    let manager = ImposterManager::new();
    mk(&manager, 19895).await;

    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("h2 client");
    let resp = client
        .get("http://127.0.0.1:19895/x")
        .send()
        .await
        .expect("h2 request must succeed");
    assert_eq!(
        resp.version(),
        reqwest::Version::HTTP_2,
        "the connection must be served over HTTP/2"
    );
    let body = resp.text().await.expect("body");
    assert_eq!(body, "h2-ok");
    let _ = manager.delete_imposter(19895).await;
}

// AC3: an imposter that can fire a TCP fault is served HTTP/1-only — TCP faults are connection-level
// and incompatible with HTTP/2 multiplexing, so an h2-prior-knowledge client must NOT negotiate h2
// against it (even for a non-fault path on the same imposter).
#[tokio::test]
async fn tcp_fault_imposter_is_http1_only() {
    let manager = ImposterManager::new();
    let cfg = serde_json::json!({
        "port": 19897, "protocol": "http", "stubs": [
            { "predicates": [{ "equals": { "path": "/ok" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] },
            { "predicates": [{ "equals": { "path": "/boom" } }],
              "responses": [{ "is": { "statusCode": 200 },
                "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } } }] }
        ]
    });
    let config = serde_json::from_value(cfg).expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // h2 prior-knowledge against the normal path must fail: the listener is HTTP/1-only.
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("h2 client");
    assert!(
        h2.get("http://127.0.0.1:19897/ok").send().await.is_err(),
        "a TCP-fault imposter must not negotiate HTTP/2"
    );
    // An ordinary HTTP/1 client still works on the non-fault path.
    let h1 = reqwest::Client::builder().build().expect("h1 client");
    let resp = h1
        .get("http://127.0.0.1:19897/ok")
        .send()
        .await
        .expect("h1 request");
    assert_eq!(resp.text().await.expect("body"), "ok");
    let _ = manager.delete_imposter(19897).await;
}

// AC2: an ordinary HTTP/1 client still works (auto-negotiation must not regress HTTP/1).
#[tokio::test]
async fn imposter_still_serves_http1() {
    let manager = ImposterManager::new();
    mk(&manager, 19896).await;

    let client = reqwest::Client::builder()
        .http1_only()
        .build()
        .expect("h1 client");
    let resp = client
        .get("http://127.0.0.1:19896/x")
        .send()
        .await
        .expect("h1 request must succeed");
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert_eq!(resp.text().await.expect("body"), "h2-ok");
    let _ = manager.delete_imposter(19896).await;
}
