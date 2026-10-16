//! Issue #1380: a proxy forwards the request's own method, as Mountebank does
//! (`httpProxy.js`: `method: originalRequest.method`). A fixed match used to send every method
//! outside GET/POST/PUT/DELETE/PATCH/HEAD upstream as GET, and record the GET answer under the
//! original method. The Host/Content-Length filter on forwarded headers was untested too.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn start() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    (
        reqwest::Client::new(),
        format!("http://{}", running.local_addr()),
        manager,
    )
}

/// Create `imposter` without a port and return the one the engine assigned.
async fn create(client: &reqwest::Client, admin: &str, imposter: Value) -> u16 {
    let response = client
        .post(format!("{admin}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST");
    assert_eq!(response.status().as_u16(), 201, "{imposter}");
    let body: Value = response.json().await.expect("json");
    body["port"].as_u64().expect("assigned port") as u16
}

async fn imposter(client: &reqwest::Client, admin: &str, port: u16) -> Value {
    client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json")
}

fn method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("valid method")
}

/// The upstream's recorded request header `name`, case-insensitively.
fn recorded_header<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
    request["headers"]
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

#[tokio::test]
async fn every_method_reaches_the_upstream_as_sent() {
    let (client, admin, manager) = start().await;
    let upstream = create(
        &client,
        &admin,
        json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "up"}}]}]}),
    )
    .await;
    let proxy = create(
        &client,
        &admin,
        json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyTransparent"
        }}]}]}),
    )
    .await;

    let sent = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "PROPFIND", "QUERY",
    ];
    for name in sent {
        let status = client
            .request(method(name), format!("http://127.0.0.1:{proxy}/m"))
            .send()
            .await
            .expect("proxied request")
            .status()
            .as_u16();
        assert_eq!(status, 200, "{name} through the proxy");
    }

    let seen: Vec<String> = imposter(&client, &admin, upstream).await["requests"]
        .as_array()
        .expect("recorded requests")
        .iter()
        .map(|r| r["method"].as_str().expect("method").to_string())
        .collect();
    assert_eq!(seen, sent);

    for port in [proxy, upstream] {
        let _ = manager.delete_imposter(port).await;
    }
}

#[tokio::test]
async fn a_proxyonce_recording_of_options_is_keyed_and_replayed_as_options() {
    let (client, admin, manager) = start().await;
    let upstream = create(
        &client,
        &admin,
        json!({"protocol": "http", "recordRequests": true, "stubs": [
            {"predicates": [{"equals": {"method": "OPTIONS"}}],
             "responses": [{"is": {"statusCode": 200, "body": "options"}}]},
            {"responses": [{"is": {"statusCode": 200, "body": "other"}}]}
        ]}),
    )
    .await;
    let proxy = create(
        &client,
        &admin,
        json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{"matches": {"method": true, "path": true}}]
        }}]}]}),
    )
    .await;

    for round in 0..2 {
        let body = client
            .request(method("OPTIONS"), format!("http://127.0.0.1:{proxy}/p"))
            .send()
            .await
            .expect("proxied OPTIONS")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "options", "round {round}");
    }

    let upstream_requests = imposter(&client, &admin, upstream).await["requests"]
        .as_array()
        .expect("recorded requests")
        .len();
    assert_eq!(upstream_requests, 1, "the second OPTIONS is replayed");

    let printed = imposter(&client, &admin, proxy).await;
    let recorded = printed["stubs"]
        .as_array()
        .expect("stubs")
        .iter()
        .find(|s| s["responses"][0].get("is").is_some())
        .expect("a recorded stub");
    let predicates = recorded["predicates"].to_string();
    assert!(
        predicates.contains(r#""method":"OPTIONS""#),
        "the recording is keyed on OPTIONS: {predicates}"
    );

    for port in [proxy, upstream] {
        let _ = manager.delete_imposter(port).await;
    }
}

#[tokio::test]
async fn the_upstream_sees_its_own_host_and_one_content_length() {
    let (client, admin, manager) = start().await;
    let upstream = create(
        &client,
        &admin,
        json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]}),
    )
    .await;
    let proxy = create(
        &client,
        &admin,
        json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyTransparent"
        }}]}]}),
    )
    .await;

    let status = client
        .post(format!("http://127.0.0.1:{proxy}/h"))
        .header("Host", "client.example")
        .header("X-Probe", "1")
        .body("hello")
        .send()
        .await
        .expect("proxied POST")
        .status()
        .as_u16();
    assert_eq!(status, 200);

    let printed = imposter(&client, &admin, upstream).await;
    let request = &printed["requests"][0];
    assert_eq!(
        recorded_header(request, "host"),
        Some(&json!(format!("127.0.0.1:{upstream}"))),
        "the client's Host is not forwarded: {request}"
    );
    assert_eq!(
        recorded_header(request, "x-probe"),
        Some(&json!("1")),
        "{request}"
    );
    assert_eq!(
        recorded_header(request, "content-length"),
        Some(&json!("5")),
        "{request}"
    );

    for port in [proxy, upstream] {
        let _ = manager.delete_imposter(port).await;
    }
}
