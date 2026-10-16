//! Issue #1500: an imposter whose requested address is held by another process answered 500
//! `internal error`; Mountebank answers 400. The caller named the address, so it is their mistake.

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    url: String,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None);
        let running = server.bind().await.expect("admin API binds");
        Admin {
            client: reqwest::Client::new(),
            url: format!("http://{}", running.local_addr()),
        }
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.url));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// A port some other process holds on 127.0.0.1, held until the listener drops.
fn hold() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    (listener, port)
}

fn message(body: &Value) -> &str {
    body["errors"][0]["message"].as_str().expect("message")
}

#[tokio::test]
async fn post_on_a_port_another_process_holds_is_a_400() {
    let admin = Admin::start().await;
    let (_held, port) = hold();

    let (status, body) = admin
        .send(
            reqwest::Method::POST,
            "/imposters",
            Some(json!({"protocol": "http", "port": port, "host": "127.0.0.1"})),
        )
        .await;

    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errors"][0]["type"], "bad data");
    assert!(message(&body).contains("already in use"), "{body}");
    let (status, _) = admin
        .send(reqwest::Method::GET, &format!("/imposters/{port}"), None)
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn post_with_an_unresolvable_host_is_a_400() {
    let admin = Admin::start().await;
    let (status, body) = admin
        .send(
            reqwest::Method::POST,
            "/imposters",
            Some(json!({"protocol": "http", "host": "no-such-host.invalid", "port": hold().1})),
        )
        .await;

    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errors"][0]["type"], "bad data");
}

#[tokio::test]
async fn put_with_a_port_another_process_holds_is_a_400_and_keeps_the_report() {
    let admin = Admin::start().await;
    let (_held, port) = hold();

    let (status, body) = admin
        .send(
            reqwest::Method::PUT,
            "/imposters",
            Some(json!({"imposters": [{"protocol": "http", "port": port, "host": "127.0.0.1"}]})),
        )
        .await;

    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errors"][0]["type"], "bad data");
    assert!(
        message(&body).starts_with("Replace partially failed: "),
        "{body}"
    );
    let failed = body["failed"][0].as_str().expect("failed entry");
    assert!(failed.starts_with(&format!("{port}: ")), "{failed}");
}
