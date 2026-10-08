//! Issue #1329: a proxy's `predicateGenerators` record Mountebank's shape — one predicate per
//! matched field, `deepEquals` for a whole field, the generator's `except` and `caseSensitive`
//! copied onto each predicate and the captured values raw. Before, `except` was applied at capture
//! and dropped, so the recorded stub compared a stripped value against an unstripped request and
//! never matched again; and one `equals` over every field let a request with extra query
//! parameters reuse a recording Mountebank would re-proxy.
//!
//! As in `issue_1327_predicate_generator_selectors.rs`, a response the recorded stub serves carries
//! no `x-rift-proxy` header; one that reaches the proxy does.

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

    /// A recording upstream and a `mode` proxy imposter in front of it with `generator`.
    async fn proxy(&self, mode: &str, generator: Value) -> (u16, u16) {
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
                "mode": mode,
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
        imposter["stubs"].as_array().cloned().unwrap_or_default()
    }

    async fn drop_all(&self, ports: &[u16]) {
        for port in ports {
            let _ = self.manager.delete_imposter(*port).await;
        }
    }
}

#[tokio::test]
async fn an_except_generator_replays_requests_differing_only_in_the_stripped_part() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true}, "except": "\\d+"}),
        )
        .await;

    assert!(!admin.served_by_recorded_stub(port, "/users/123", "").await);
    assert!(
        admin.served_by_recorded_stub(port, "/users/456", "").await,
        "the recorded stub strips `except` from both sides"
    );
    let stubs = admin.stubs(port).await;
    assert_eq!(
        stubs[0]["predicates"],
        json!([{"deepEquals": {"path": "/users/123"}, "except": "\\d+"}])
    );
    assert!(admin.served_by_recorded_stub(port, "/users/123", "").await);
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    assert_eq!(
        admin.stubs(port).await.len(),
        2,
        "one recording, then the proxy"
    );
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_whole_query_matcher_does_not_serve_a_request_with_extra_parameters() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true, "query": true}}),
        )
        .await;

    assert!(!admin.served_by_recorded_stub(port, "/x?a=1", "").await);
    assert!(admin.served_by_recorded_stub(port, "/x?a=1", "").await);
    assert!(
        !admin.served_by_recorded_stub(port, "/x?a=1&b=2", "").await,
        "deepEquals on the query: an extra parameter is a different request"
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn proxy_always_groups_responses_per_distinct_request() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy(
            "proxyAlways",
            json!({"matches": {"method": true, "path": true}}),
        )
        .await;

    for path in ["/a", "/b", "/a"] {
        assert!(!admin.served_by_recorded_stub(port, path, "").await);
    }
    let stubs = admin.stubs(port).await;
    let groups: Vec<(Value, usize)> = stubs[1..]
        .iter()
        .map(|stub| {
            (
                stub["predicates"].clone(),
                stub["responses"].as_array().map_or(0, Vec::len),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (
                json!([{"deepEquals": {"method": "POST"}}, {"deepEquals": {"path": "/b"}}]),
                1
            ),
            (
                json!([{"deepEquals": {"method": "POST"}}, {"deepEquals": {"path": "/a"}}]),
                2
            ),
        ]
    );
    assert_eq!(admin.upstream_hits(upstream).await, 3);
    admin.drop_all(&[port, upstream]).await;
}

/// A generator without `caseSensitive` records a predicate that ignores case, as Mountebank's does.
#[tokio::test]
async fn a_recording_without_case_sensitive_ignores_case() {
    let admin = Admin::start().await;
    let (port, upstream) = admin
        .proxy("proxyOnce", json!({"matches": {"path": true}}))
        .await;

    assert!(!admin.served_by_recorded_stub(port, "/users", "").await);
    assert!(admin.served_by_recorded_stub(port, "/USERS", "").await);
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    admin.drop_all(&[port, upstream]).await;
}
