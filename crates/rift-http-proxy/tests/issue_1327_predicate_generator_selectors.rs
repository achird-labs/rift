//! Issue #1327: a proxy's `predicateGenerators` may carry Mountebank's `jsonpath`, `xpath` and
//! `ignore`. Rift accepted the keys and dropped them, so a recorded stub matched the whole body
//! (or every query parameter) and a request differing anywhere else missed it.
//!
//! A response the recorded stub serves carries no `x-rift-proxy` header; one that reaches the
//! proxy does, and is forwarded and recorded as a new stub (issue #1333).

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
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None);
        let running = server.bind().await.expect("admin API binds");
        let url = format!("http://{}", running.local_addr());
        Admin {
            client: reqwest::Client::new(),
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

    async fn upstream_hits(&self, port: u16) -> usize {
        let imposter: Value = self
            .client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        imposter["requests"].as_array().map_or(0, Vec::len)
    }

    /// A recording upstream and a `proxyOnce` imposter in front of it with `generator`.
    async fn proxy(&self, generator: Value) -> (u16, u16) {
        let upstream = free_port();
        self.create(
            json!({"port": upstream, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"body": "up"}}]}]}),
        )
        .await;
        let port = free_port();
        self.create(
            json!({"port": port, "protocol": "http", "stubs": [{"responses": [{"proxy": {
                "to": format!("http://127.0.0.1:{upstream}"),
                "mode": "proxyOnce",
                "predicateGenerators": [generator]
            }}]}]}),
        )
        .await;
        (port, upstream)
    }

    /// Sends a request and answers whether a recorded stub served it.
    async fn served_by_recorded_stub(&self, port: u16, path_and_query: &str, body: &str) -> bool {
        let response = self
            .client
            .post(format!("http://127.0.0.1:{port}{path_and_query}"))
            .body(body.to_string())
            .send()
            .await
            .expect("request");
        let recorded = !response.headers().contains_key("x-rift-proxy");
        assert_eq!(response.text().await.expect("body"), "up");
        recorded
    }

    async fn drop_all(&self, ports: &[u16]) {
        for port in ports {
            let _ = self.manager.delete_imposter(*port).await;
        }
    }
}

#[tokio::test]
async fn a_jsonpath_generator_replays_on_the_selected_value_only() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(json!({"matches": {"path": true, "body": true}, "jsonpath": {"selector": "$.id"}}))
        .await;

    assert!(
        !admin
            .served_by_recorded_stub(port, "/orders", r#"{"id": 1, "ts": "a"}"#)
            .await
    );
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    // Same `$.id`, different body elsewhere: the recorded stub answers.
    assert!(
        admin
            .served_by_recorded_stub(port, "/orders", r#"{"id": 1, "ts": "b"}"#)
            .await
    );
    // A different `$.id` misses it, and is forwarded.
    assert!(
        !admin
            .served_by_recorded_stub(port, "/orders", r#"{"id": 2, "ts": "a"}"#)
            .await
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn an_xpath_generator_replays_on_the_selected_value_only() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(json!({"matches": {"body": true},
            "xpath": {"selector": "//a:id", "ns": {"a": "urn:a"}}}))
        .await;
    let order = |id: u32, ts: &str| {
        format!(r#"<a:order xmlns:a="urn:a"><a:id>{id}</a:id><a:ts>{ts}</a:ts></a:order>"#)
    };

    assert!(
        !admin
            .served_by_recorded_stub(port, "/", &order(7, "a"))
            .await
    );
    assert!(
        admin
            .served_by_recorded_stub(port, "/", &order(7, "b"))
            .await
    );
    assert!(
        !admin
            .served_by_recorded_stub(port, "/", &order(8, "a"))
            .await
    );
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn an_ignored_query_parameter_does_not_split_the_recording() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(json!({"matches": {"path": true, "query": true}, "ignore": {"query": "ts"}}))
        .await;

    assert!(
        !admin
            .served_by_recorded_stub(port, "/search?q=rust&ts=1", "")
            .await
    );
    assert!(
        admin
            .served_by_recorded_stub(port, "/search?q=rust&ts=2", "")
            .await
    );
    // A different `q` is a different query, and a new recording.
    assert!(
        !admin
            .served_by_recorded_stub(port, "/search?q=go&ts=3", "")
            .await
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn an_unread_generator_key_is_reported_and_the_imposter_loads() {
    let admin = Admin::start().await;
    let port = free_port();
    let created = admin
        .create(
            json!({"port": port, "protocol": "http", "stubs": [{"responses": [{"proxy": {
                "to": "http://127.0.0.1:9",
                "predicateGenerators": [{"matchs": {"path": true}}]
            }}]}]}),
        )
        .await;
    let warnings = created["_rift"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w["warningType"] == "config_key_ignored"
                && w["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("`matchs`"))),
        "{created}"
    );
    admin.drop_all(&[port]).await;
}
