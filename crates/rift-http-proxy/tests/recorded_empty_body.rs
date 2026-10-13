//! Issue #1365: a request without a body is recorded with `"body": ""`, as Mountebank records it,
//! not with the key absent. Declarative `exists` on `body` keeps treating it as absent.

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

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

#[tokio::test]
async fn a_bodyless_request_is_recorded_with_an_empty_string_body() {
    let (client, admin, manager) = start().await;
    let port = free_port();
    let created = client
        .post(format!("{admin}/imposters"))
        .json(
            &json!({"port": port, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]}),
        )
        .send()
        .await
        .expect("POST");
    assert_eq!(created.status().as_u16(), 201);

    let url = format!("http://127.0.0.1:{port}/r");
    client.get(&url).send().await.expect("GET");
    client
        .post(&url)
        .header("Content-Length", "0")
        .send()
        .await
        .expect("POST, empty");
    client.post(&url).body("a").send().await.expect("POST a");

    let printed: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("json");
    let bodies: Vec<Value> = printed["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|r| r.get("body").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(bodies, vec![json!(""), json!(""), json!("a")]);

    let _ = manager.delete_imposter(port).await;
}

/// Regression guard: the empty string is only what scripts and the journal see; `exists` still
/// reads a body-less request as having no body, as Mountebank does.
#[tokio::test]
async fn exists_on_body_still_treats_a_bodyless_request_as_absent() {
    let (client, admin, manager) = start().await;
    let port = free_port();
    let created = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": port, "protocol": "http", "stubs": [
            {"predicates": [{"exists": {"body": true}}], "responses": [{"is": {"body": "has"}}]},
            {"predicates": [{"exists": {"body": false}}], "responses": [{"is": {"body": "none"}}]}
        ]}))
        .send()
        .await
        .expect("POST");
    assert_eq!(created.status().as_u16(), 201);

    let body = client
        .get(format!("http://127.0.0.1:{port}/e"))
        .send()
        .await
        .expect("GET")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "none");

    let _ = manager.delete_imposter(port).await;
}
