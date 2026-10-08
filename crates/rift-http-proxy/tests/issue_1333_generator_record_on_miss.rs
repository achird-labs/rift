//! Issue #1333: with `predicateGenerators`, a `proxyOnce` request that misses every recorded stub is
//! forwarded and recorded as a new stub, as in Mountebank's `proxyAndRecord`. The proxy store keyed
//! a generator's claim on method, path and query only, so the second variant on a path collided
//! with the first recording and was answered from the store: never forwarded, never recorded.
//!
//! A response a recorded stub serves carries no `x-rift-proxy` header; one that reaches the proxy
//! does. The upstream's journal counts the forwards.

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use rift_mock_core::recording::{
    ClaimOutcome, ClaimToken, LocalProxyStore, ProxyMode, ProxyRecordingStore, ProxyStoreError,
    RecordedResponse, RequestSignature, StubPublication,
};
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
    async fn start(manager: ImposterManager) -> Self {
        let manager = Arc::new(manager);
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

    async fn create(&self, imposter: Value) {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
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

    async fn upstream_hits(&self, port: u16) -> usize {
        self.imposter(port).await["requests"]
            .as_array()
            .map_or(0, Vec::len)
    }

    async fn stubs(&self, port: u16) -> Vec<Value> {
        self.imposter(port).await["stubs"]
            .as_array()
            .cloned()
            .expect("stubs array")
    }

    /// A recording upstream answering `upstream_response`, and a proxy in front of it.
    async fn proxy(&self, mode: &str, generator: Value, upstream_response: Value) -> (u16, u16) {
        let upstream = free_port();
        self.create(
            json!({"port": upstream, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [upstream_response]}]}),
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
    async fn served_by_recorded_stub(
        &self,
        port: u16,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> bool {
        let mut request = self
            .client
            .post(format!("http://127.0.0.1:{port}{path}"))
            .body(body.to_string());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.expect("request");
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

fn up() -> Value {
    json!({"is": {"body": "up"}})
}

#[tokio::test]
async fn a_jsonpath_miss_is_forwarded_and_recorded() {
    let admin = Admin::start(ImposterManager::new()).await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true, "body": true}, "jsonpath": {"selector": "$.id"}}),
            up(),
        )
        .await;

    assert!(
        !admin
            .served_by_recorded_stub(port, "/orders", &[], r#"{"id": 1}"#)
            .await
    );
    assert!(
        !admin
            .served_by_recorded_stub(port, "/orders", &[], r#"{"id": 2}"#)
            .await
    );
    assert_eq!(
        admin.upstream_hits(upstream).await,
        2,
        "a request missing the recorded stub must reach the upstream"
    );
    // Both identities now have their own recorded stub.
    assert!(
        admin
            .served_by_recorded_stub(port, "/orders", &[], r#"{"id": 2}"#)
            .await
    );
    assert!(
        admin
            .served_by_recorded_stub(port, "/orders", &[], r#"{"id": 1}"#)
            .await
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(
        admin.stubs(port).await.len(),
        3,
        "two recorded stubs plus the proxy"
    );
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_header_generator_records_one_stub_per_tenant() {
    let admin = Admin::start(ImposterManager::new()).await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true, "headers": {"X-Tenant": true}}}),
            up(),
        )
        .await;

    for tenant in ["a", "b"] {
        assert!(
            !admin
                .served_by_recorded_stub(port, "/t", &[("X-Tenant", tenant)], "")
                .await,
            "tenant {tenant} is a new identity"
        );
    }
    for tenant in ["a", "b"] {
        assert!(
            admin
                .served_by_recorded_stub(port, "/t", &[("X-Tenant", tenant)], "")
                .await,
            "tenant {tenant} replays from its own recorded stub"
        );
    }
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(admin.stubs(port).await.len(), 3);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_body_generator_records_one_stub_per_body() {
    let admin = Admin::start(ImposterManager::new()).await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true, "body": true}}),
            up(),
        )
        .await;

    assert!(!admin.served_by_recorded_stub(port, "/o", &[], "one").await);
    assert!(!admin.served_by_recorded_stub(port, "/o", &[], "two").await);
    assert!(admin.served_by_recorded_stub(port, "/o", &[], "one").await);
    assert!(admin.served_by_recorded_stub(port, "/o", &[], "two").await);
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(admin.stubs(port).await.len(), 3);
    admin.drop_all(&[port, upstream]).await;
}

/// proxyAlways forwards every request; equal generated predicates merge into one recorded stub.
#[tokio::test]
async fn proxy_always_merges_per_generated_identity() {
    let admin = Admin::start(ImposterManager::new()).await;
    let (port, upstream) = admin
        .proxy(
            "proxyAlways",
            json!({"matches": {"path": true, "body": true}, "jsonpath": {"selector": "$.id"}}),
            up(),
        )
        .await;

    for body in [r#"{"id": 1}"#, r#"{"id": 1, "x": 0}"#, r#"{"id": 2}"#] {
        assert!(!admin.served_by_recorded_stub(port, "/o", &[], body).await);
    }
    assert_eq!(admin.upstream_hits(upstream).await, 3);
    let stubs = admin.stubs(port).await;
    assert_eq!(
        stubs.len(),
        3,
        "the proxy plus one stub per $.id: {stubs:?}"
    );
    let mut response_counts: Vec<usize> = stubs[1..]
        .iter()
        .map(|s| s["responses"].as_array().map_or(0, Vec::len))
        .collect();
    response_counts.sort_unstable();
    assert_eq!(response_counts, vec![1, 2]);
    admin.drop_all(&[port, upstream]).await;
}

/// Delegates to the built-in store and counts what the engine asked of it.
struct CountingStore {
    inner: LocalProxyStore,
    completes: Mutex<HashMap<RequestSignature, usize>>,
    in_flight: Mutex<usize>,
}

impl ProxyRecordingStore for CountingStore {
    fn try_claim(
        &self,
        port: u16,
        sig: &RequestSignature,
    ) -> Result<ClaimOutcome, ProxyStoreError> {
        let outcome = self.inner.try_claim(port, sig)?;
        if outcome == ClaimOutcome::InFlight {
            *self.in_flight.lock().expect("lock") += 1;
        }
        Ok(outcome)
    }

    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
        self.inner.release_claim(port, sig, token);
    }

    fn record(
        &self,
        port: u16,
        sig: RequestSignature,
        token: ClaimToken,
        resp: RecordedResponse,
    ) -> Result<(), ProxyStoreError> {
        self.inner.record(port, sig, token, resp)
    }

    fn complete(
        &self,
        port: u16,
        sig: RequestSignature,
        token: ClaimToken,
        resp: RecordedResponse,
        publication: &StubPublication<'_>,
    ) -> Result<(), ProxyStoreError> {
        *self
            .completes
            .lock()
            .expect("lock")
            .entry(sig.clone())
            .or_default() += 1;
        self.inner.complete(port, sig, token, resp, publication)
    }

    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
        self.inner.lookup(port, sig)
    }

    fn clear(&self, port: u16) {
        self.inner.clear(port);
    }
}

/// Five simultaneous requests for a new identity, behind a slow upstream: exactly one wins the
/// claim and records; the rest are `InFlight`. Record-on-miss must not bypass the claim gate.
#[tokio::test]
async fn concurrent_misses_for_one_identity_record_it_exactly_once() {
    let store = Arc::new(CountingStore {
        inner: LocalProxyStore::new(ProxyMode::ProxyOnce),
        completes: Mutex::new(HashMap::new()),
        in_flight: Mutex::new(0),
    });
    let admin = Admin::start(ImposterManager::new().with_proxy_store(store.clone())).await;
    let (port, upstream) = admin
        .proxy(
            "proxyOnce",
            json!({"matches": {"path": true, "body": true}, "jsonpath": {"selector": "$.id"}}),
            json!({"is": {"body": "up"}, "_behaviors": {"wait": 500}}),
        )
        .await;

    assert!(
        !admin
            .served_by_recorded_stub(port, "/orders", &[], r#"{"id": 1}"#)
            .await
    );

    let mut requests = tokio::task::JoinSet::new();
    for n in 0..5 {
        let client = admin.client.clone();
        requests.spawn(async move {
            client
                .post(format!("http://127.0.0.1:{port}/orders"))
                .body(format!(r#"{{"id": 2, "n": {n}}}"#))
                .send()
                .await
                .expect("request")
                .text()
                .await
                .expect("body")
        });
    }
    while let Some(body) = requests.join_next().await {
        assert_eq!(body.expect("task"), "up");
    }

    let completes = store.completes.lock().expect("lock").clone();
    assert_eq!(
        completes.len(),
        2,
        "one recorded identity per $.id, each settled through complete(): {completes:?}"
    );
    assert!(
        completes.values().all(|n| *n == 1),
        "each identity is recorded exactly once: {completes:?}"
    );
    // A request landing after the winner settled is served by its stub and never claims, so the
    // loser count is a range; every loser is forwarded.
    let in_flight = *store.in_flight.lock().expect("lock");
    assert!(
        (1..=4).contains(&in_flight),
        "concurrent losers see the winner's claim in flight: {in_flight}"
    );
    assert_eq!(admin.upstream_hits(upstream).await, 2 + in_flight);
    assert_eq!(
        admin.stubs(port).await.len(),
        3,
        "one recorded stub per $.id plus the proxy; a loser records nothing"
    );
    admin.drop_all(&[port, upstream]).await;
}

/// A generator that cannot produce predicates records no stub (issue #498) and skips the store:
/// every such request is forwarded and tagged, never answered with another request's recording.
/// Created through the manager because the admin API refuses `inject` without `--allowInjection`.
#[tokio::test]
async fn a_failed_generation_forwards_every_request() {
    let admin = Admin::start(ImposterManager::new()).await;
    let upstream = free_port();
    admin
        .create(
            json!({"port": upstream, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [up()]}]}),
        )
        .await;
    let port = free_port();
    let config = serde_json::from_value(json!({"port": port, "protocol": "http",
    "stubs": [{"responses": [{"proxy": {
        "to": format!("http://127.0.0.1:{upstream}"),
        "mode": "proxyOnce",
        "predicateGenerators": [{"inject":
            "function(config, logger, predicates) { throw new Error('boom'); }"}]
    }}]}]}))
    .expect("imposter config");
    admin
        .manager
        .create_imposter(config)
        .await
        .expect("create proxy");

    for path in ["/gen", "/gen", "/gen?x=1"] {
        let response = admin
            .client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            response
                .headers()
                .get("x-rift-generator-error")
                .and_then(|v| v.to_str().ok()),
            Some("script-error"),
            "{path}"
        );
        assert_eq!(response.text().await.expect("body"), "up");
    }
    assert_eq!(admin.upstream_hits(upstream).await, 3);
    assert_eq!(admin.stubs(port).await.len(), 1, "no stub was recorded");
    admin.drop_all(&[port, upstream]).await;
}
