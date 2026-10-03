//! Issue #1239: `DELETE /imposters/{port}/savedProxyResponses` cleared only the no-generator proxy
//! store. A `proxyOnce`/`proxyAlways` that records through `predicateGenerators` (or
//! `addWaitBehavior`/`addDecorateBehavior`) records a *stub*, and that stub stayed, so the imposter
//! kept replaying it and never reached the upstream again. Mountebank's `deleteSavedProxyResponses`
//! removes the recorded responses and drops the stubs they leave empty; Rift marks a recorded stub
//! with `recordedFrom`.

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

struct Admin {
    client: reqwest::Client,
    url: String,
    manager: Arc<ImposterManager>,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let server =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .with_allow_injection(true);
        let running = server.bind().await.expect("admin API binds");
        let url = format!("http://{}", running.local_addr());
        let client = reqwest::Client::new();
        Admin {
            client,
            url,
            manager,
        }
    }

    async fn create(&self, imposter: Value) -> Value {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
        response.json().await.expect("json")
    }

    async fn imposter(&self, port: u16) -> Value {
        self.client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json")
    }

    /// How many requests the upstream imposter has received.
    async fn upstream_hits(&self, port: u16) -> usize {
        self.imposter(port).await["requests"]
            .as_array()
            .map_or(0, Vec::len)
    }

    /// An upstream that answers `up` and records what it receives.
    async fn upstream(&self, response: Value) -> u16 {
        let port = free_port();
        self.create(
            json!({"port": port, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [response]}]}),
        )
        .await;
        port
    }

    async fn get(&self, port: u16) -> reqwest::Response {
        self.client
            .get(format!("http://127.0.0.1:{port}/p"))
            .send()
            .await
            .expect("GET")
    }

    async fn drop_all(&self, ports: &[u16]) {
        for port in ports {
            let _ = self.manager.delete_imposter(*port).await;
        }
    }

    async fn clear_saved_proxy_responses(&self, port: u16) -> Value {
        let response = self
            .client
            .delete(format!("{}/imposters/{port}/savedProxyResponses", self.url))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(response.status().as_u16(), 200);
        response.json().await.expect("json")
    }

    async fn stubs(&self, port: u16) -> Vec<Value> {
        self.imposter(port).await["stubs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn recorded(stubs: &[Value]) -> usize {
    stubs
        .iter()
        .filter(|s| s.get("recordedFrom").is_some())
        .count()
}

fn recording_proxy(port: u16, upstream: u16, mode: &str) -> Value {
    json!({"port": port, "protocol": "http", "stubs": [
        {"predicates": [{"equals": {"path": "/kept"}}], "responses": [{"is": {"body": "mine"}}]},
        {"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": mode,
            "predicateGenerators": [{"matches": {"path": true}}]
        }}]}
    ]})
}

async fn clearing_removes_the_recorded_stubs(mode: &str) {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin.create(recording_proxy(port, upstream, mode)).await;

    assert_eq!(admin.get(port).await.text().await.expect("body"), "up");
    let before = admin.stubs(port).await;
    assert_eq!(
        recorded(&before),
        1,
        "the proxy recorded a stub: {before:?}"
    );
    assert_eq!(admin.upstream_hits(upstream).await, 1);

    let body = admin.clear_saved_proxy_responses(port).await;
    let answered = body["stubs"].as_array().cloned().unwrap_or_default();
    assert_eq!(recorded(&answered), 0, "the DELETE body: {body}");

    let after = admin.stubs(port).await;
    assert_eq!(recorded(&after), 0, "the recorded stub is gone: {after:?}");
    assert_eq!(after.len(), 2, "the authored stubs are kept: {after:?}");
    assert_eq!(after[0]["responses"][0]["is"]["body"], "mine");
    assert!(after[1]["responses"][0].get("proxy").is_some(), "{after:?}");

    assert_eq!(admin.get(port).await.text().await.expect("body"), "up");
    assert_eq!(
        admin.upstream_hits(upstream).await,
        2,
        "after the clear the proxy reaches the upstream again"
    );

    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn clearing_removes_a_proxy_once_recorded_stub() {
    clearing_removes_the_recorded_stubs("proxyOnce").await;
}

#[tokio::test]
async fn clearing_removes_a_proxy_always_recorded_stub() {
    clearing_removes_the_recorded_stubs("proxyAlways").await;
}

/// An imposter with nothing recorded answers the same stubs before and after.
#[tokio::test]
async fn clearing_with_nothing_recorded_keeps_every_stub() {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin
        .create(recording_proxy(port, upstream, "proxyOnce"))
        .await;

    let before = admin.stubs(port).await;
    admin.clear_saved_proxy_responses(port).await;
    assert_eq!(admin.stubs(port).await, before);

    admin.drop_all(&[port, upstream]).await;
}
