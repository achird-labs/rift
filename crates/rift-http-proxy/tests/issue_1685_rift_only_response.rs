//! Issue #1685: a response holding only a `_rift` block, with no `script` (for example the
//! documented `{"_rift": {"fault": {"tcp": "malformed"}}}`), matched its stub and was then answered
//! as a no-match (`200`, `x-rift-no-match`): its fault, `stateOps` and `conditional` never ran. It
//! is now served as `is: {}` carrying those effects, completed by `defaultResponse`.
//!
//! The stored form is unchanged: the response still reads back with no `is` key.

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn imposter(manager: &ImposterManager, config: Value) -> u16 {
    let config = serde_json::from_value(config).expect("imposter config");
    let port = manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// A one-stub imposter whose only response is `response`.
fn one_response(response: Value) -> Value {
    json!({ "protocol": "http", "stubs": [{ "responses": [response] }] })
}

/// No connection pool: a pooled connection may outlive a faulted one's imposter.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .http1_only()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client")
}

/// Write one raw HTTP/1.1 GET and read until the server closes (or resets) the connection.
async fn raw_get(port: u16) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET /x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out))
        .await
        .map_err(|_| std::io::Error::other("read timed out"))??;
    Ok(out)
}

fn assert_not_a_no_match(response: &reqwest::Response) {
    assert!(
        response.headers().get("x-rift-no-match").is_none(),
        "the stub matched, so the answer must not be a no-match: {:?}",
        response.headers()
    );
}

#[tokio::test]
async fn rift_only_tcp_malformed_fires() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        one_response(json!({ "_rift": { "fault": { "tcp": "malformed" } } })),
    )
    .await;

    let bytes = raw_get(port).await.expect("a malformed chunk, then close");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "status line first: {text:?}"
    );
    assert!(
        text.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "a chunked body follows: {text:?}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("x-rift-no-match"),
        "the fault must fire, not a no-match: {text:?}"
    );
}

#[tokio::test]
async fn rift_only_tcp_reset_fires() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        one_response(json!({ "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } } })),
    )
    .await;

    match raw_get(port).await {
        Err(_) => {}
        Ok(bytes) => assert!(
            bytes.is_empty(),
            "a reset sends no HTTP answer: {:?}",
            String::from_utf8_lossy(&bytes)
        ),
    }
}

#[tokio::test]
async fn rift_only_error_fault_serves_its_status() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        one_response(json!({ "_rift": { "fault": { "error": {
            "probability": 1.0, "status": 503, "body": "down"
        } } } })),
    )
    .await;

    let response = client()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 503);
    assert_not_a_no_match(&response);
    assert_eq!(response.text().await.expect("body"), "down");
}

#[tokio::test]
async fn rift_only_latency_fault_serves_empty_200() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        one_response(json!({ "_rift": { "fault": { "latency": {
            "probability": 1.0, "ms": 300
        } } } })),
    )
    .await;

    let started = Instant::now();
    let response = client()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send");
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 200);
    assert_not_a_no_match(&response);
    assert_eq!(response.text().await.expect("body"), "");
    assert!(
        elapsed >= Duration::from_millis(300),
        "the latency fault must delay the answer: {elapsed:?}"
    );
}

#[tokio::test]
async fn rift_only_response_is_completed_by_default_response() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": { "statusCode": 404, "body": "dflt" },
            "stubs": [{ "responses": [
                { "_rift": { "fault": { "latency": { "probability": 1.0, "ms": 300 } } } }
            ] }]
        }),
    )
    .await;

    let started = Instant::now();
    let response = client()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send");
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 404);
    assert!(
        response.headers().get("x-rift-default-response").is_none(),
        "a matched stub completed by defaultResponse is not the no-match default: {:?}",
        response.headers()
    );
    assert_eq!(response.text().await.expect("body"), "dflt");
    assert!(
        elapsed >= Duration::from_millis(300),
        "the stub's latency fault must run before the default completes it: {elapsed:?}"
    );
}

#[tokio::test]
async fn rift_only_state_ops_run() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        json!({
            "protocol": "http",
            "_rift": { "flowState": { "flowIdSource": "header:X-Session" } },
            "stubs": [{ "responses": [
                { "_rift": { "stateOps": [{ "op": "increment", "key": "hits" }] } }
            ] }]
        }),
    )
    .await;

    let client = client();
    for _ in 0..2 {
        let response = client
            .get(format!("http://127.0.0.1:{port}/x"))
            .header("X-Session", "s-1685")
            .send()
            .await
            .expect("send");
        assert_eq!(response.status(), 200);
        assert_not_a_no_match(&response);
    }
    let hits = manager
        .get_imposter(port)
        .expect("imposter exists")
        .flow_store
        .get("s-1685", "hits")
        .expect("store read");
    assert_eq!(hits, Some(json!(2)));
}

#[tokio::test]
async fn empty_rift_block_is_an_empty_200() {
    let manager = ImposterManager::new();
    let port = imposter(&manager, one_response(json!({ "_rift": {} }))).await;

    let response = client()
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);
    assert_not_a_no_match(&response);
    assert_eq!(response.text().await.expect("body"), "");
}

async fn start_admin(manager: Arc<ImposterManager>) -> (RunningAdminApi, String) {
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .bind()
        .await
        .expect("bind admin API");
    let base = format!("http://{}", running.local_addr());
    (running, base)
}

/// The fix is serve-side only: the stored response keeps no `is` key, so `GET /imposters`,
/// `rift save` and rift-cluster's replayed bytes (and its `stub_key`) are unchanged.
#[tokio::test]
async fn rift_only_round_trips_without_is() {
    let manager = Arc::new(ImposterManager::new());
    let (admin, base) = start_admin(manager.clone()).await;
    let response = json!({ "_rift": { "fault": { "tcp": "malformed" } } });
    let client = client();
    let created = client
        .post(format!("{base}/imposters"))
        .json(&one_response(response.clone()))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status(), 201);
    let port = created.json::<Value>().await.expect("json")["port"]
        .as_u64()
        .expect("port");

    let shown: Value = client
        .get(format!("{base}/imposters/{port}"))
        .send()
        .await
        .expect("GET /imposters/:port")
        .json()
        .await
        .expect("json");
    // The read-back `_rift` also prints `templated: false`, as every `_rift` block does today;
    // that predates #1685 and is the same on an `is` response.
    let shown_response = &shown["stubs"][0]["responses"][0];
    assert_eq!(
        shown_response,
        &json!({ "_rift": { "fault": { "tcp": "malformed" }, "templated": false } }),
        "{shown}"
    );
    assert!(shown_response.get("is").is_none(), "{shown}");
    admin.shutdown().await;
}
