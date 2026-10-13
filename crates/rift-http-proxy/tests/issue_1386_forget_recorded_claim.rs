//! Issue #1386: a `proxyOnce` recording lives as long as the stub it was recorded as. With that
//! stub deleted, or the stubs replaced without it, the next request for the identity is forwarded
//! and recorded again, as in Mountebank, where the stub is the recording. Before, the proxy store's
//! claim outlived the stub and the old upstream response was replayed, never forwarded.
//!
//! Every imposter takes an auto-assigned port: a fixed one would be shared with any other test
//! process bound to it (listeners use `SO_REUSEPORT`).

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    url: String,
    manager: Arc<ImposterManager>,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let server =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None);
        let running = server.bind().await.expect("admin API binds");
        Admin {
            client: reqwest::Client::new(),
            url: format!("http://{}", running.local_addr()),
            manager,
        }
    }

    /// Creates an imposter and returns the port it was given.
    async fn create(&self, imposter: Value) -> u16 {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
        let body: Value = response.json().await.expect("json");
        u16::try_from(body["port"].as_u64().expect("port")).expect("a port")
    }

    async fn stubs(&self, port: u16) -> Vec<Value> {
        let imposter: Value = self
            .client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        imposter["stubs"].as_array().cloned().expect("stubs")
    }

    async fn upstream_hits(&self, upstream: u16) -> usize {
        self.manager
            .get_imposter(upstream)
            .expect("upstream")
            .get_recorded_requests()
            .len()
    }

    async fn put_stubs(&self, port: u16, stubs: Vec<Value>) {
        let response = self
            .client
            .put(format!("{}/imposters/{port}/stubs", self.url))
            .json(&json!({ "stubs": stubs }))
            .send()
            .await
            .expect("PUT stubs");
        assert_eq!(response.status().as_u16(), 200);
    }

    /// An upstream answering `up`, and a `proxyOnce` proxy in front of it generating on the path.
    async fn proxy(&self) -> (u16, u16) {
        let upstream = self
            .create(json!({"protocol": "http", "recordRequests": true,
                "stubs": [{"responses": [{"is": {"body": "up"}}]}]}))
            .await;
        let port = self
            .create(
                json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
                    "to": format!("http://127.0.0.1:{upstream}"),
                    "mode": "proxyOnce",
                    "predicateGenerators": [{"matches": {"path": true}}]
                }}]}]}),
            )
            .await;
        (port, upstream)
    }

    /// Sends `POST /a` and answers whether it reached the proxy stub (`x-rift-proxy`).
    async fn post_reached_proxy(&self, port: u16) -> bool {
        let response = self
            .client
            .post(format!("http://127.0.0.1:{port}/a"))
            .send()
            .await
            .expect("request");
        let proxied = response.headers().contains_key("x-rift-proxy");
        assert_eq!(response.text().await.expect("body"), "up");
        proxied
    }
}

#[tokio::test]
async fn deleting_the_recorded_stub_records_the_identity_again() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.proxy().await;
    assert!(admin.post_reached_proxy(port).await);
    assert_eq!(
        admin.stubs(port).await.len(),
        2,
        "recording in front of the proxy"
    );

    let response = admin
        .client
        .delete(format!("{}/imposters/{port}/stubs/0", admin.url))
        .send()
        .await
        .expect("DELETE stub");
    assert_eq!(response.status().as_u16(), 200);

    assert!(admin.post_reached_proxy(port).await);
    assert_eq!(admin.upstream_hits(upstream).await, 2, "forwarded again");
    assert_eq!(admin.stubs(port).await.len(), 2, "recorded again");
    assert!(
        !admin.post_reached_proxy(port).await,
        "the new recording answers the next request"
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.manager.delete_all().await;
}

#[tokio::test]
async fn replacing_the_stubs_without_the_recording_records_the_identity_again() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.proxy().await;
    assert!(admin.post_reached_proxy(port).await);

    let mut stubs = admin.stubs(port).await;
    stubs.remove(0);
    admin.put_stubs(port, stubs).await;

    assert!(admin.post_reached_proxy(port).await);
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.manager.delete_all().await;
}

// Round-tripping the stubs keeps the recording in front of the proxy, so the identity is still
// answered by it and never reaches the proxy, whatever the store holds.
#[tokio::test]
async fn round_tripping_the_stubs_keeps_answering_from_the_recording() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.proxy().await;
    assert!(admin.post_reached_proxy(port).await);

    let stubs = admin.stubs(port).await;
    admin.put_stubs(port, stubs).await;

    assert!(!admin.post_reached_proxy(port).await);
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    admin.manager.delete_all().await;
}
