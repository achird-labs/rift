//! Issue #1668: a proxy's `predicateGenerators` `xpath` selector that fails to evaluate on an XML
//! body (a prefix its `ns` does not bind, an unknown function) recorded the body as `""` and
//! answered 200. Mountebank's `xpathSelect` throws while recording, after the upstream call, and
//! its HTTP server answers `500 malformed xpath predicate selector`. Rift now forwards, records
//! nothing and answers the same 500, in every proxy mode.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

/// The port the engine assigned, from a 201 body.
fn port_of(body: &Value) -> u16 {
    body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("assigned port")
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

    async fn stub_count(&self, port: u16) -> usize {
        self.imposter(port).await["stubs"]
            .as_array()
            .map_or(0, Vec::len)
    }

    /// A recording upstream and a proxy imposter in front of it, in `mode`, whose one generator
    /// captures the body through `selector`.
    async fn proxy(&self, mode: &str, selector: &str) -> (u16, u16) {
        let upstream = port_of(
            &self
                .create(json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"body": "up"}}]}]}))
                .await,
        );
        let port = port_of(
            &self
                .create(
                    json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
                        "to": format!("http://127.0.0.1:{upstream}"),
                        "mode": mode,
                        "predicateGenerators": [
                            {"matches": {"body": true}, "xpath": {"selector": selector}}
                        ]
                    }}]}]}),
                )
                .await,
        );
        (port, upstream)
    }

    async fn post(&self, port: u16, body: &str) -> (u16, String) {
        let response = self
            .client
            .post(format!("http://127.0.0.1:{port}/orders"))
            .body(body.to_string())
            .send()
            .await
            .expect("request");
        let status = response.status().as_u16();
        (status, response.text().await.expect("body"))
    }

    async fn drop_all(&self, ports: &[u16]) {
        for port in ports {
            let _ = self.manager.delete_imposter(*port).await;
        }
    }
}

/// Mountebank's answer: `500` with `{"errors":[{"code":"bad data", "message":"malformed xpath
/// predicate selector"}]}`.
fn assert_malformed_xpath(status: u16, body: &str) {
    assert_eq!(status, 500, "{body}");
    let body: Value = serde_json::from_str(body).expect("JSON error body");
    assert_eq!(
        body["errors"][0]["message"], "malformed xpath predicate selector",
        "{body}"
    );
    assert_eq!(body["errors"][0]["code"], "bad data", "{body}");
}

/// Two identical requests, each forwarded once and failed with the 500, recording nothing.
async fn assert_forwards_and_fails_twice(admin: &Admin, mode: &str, selector: &str) {
    let (port, upstream) = admin.proxy(mode, selector).await;
    let stubs = admin.stub_count(port).await;
    for sent in 1..=2 {
        let (status, body) = admin.post(port, "<a><id>1</id></a>").await;
        assert_malformed_xpath(status, &body);
        assert_eq!(
            admin.upstream_hits(upstream).await,
            sent,
            "{mode}: the request is forwarded before the generator fails, as in Mountebank"
        );
        assert_eq!(
            admin.stub_count(port).await,
            stubs,
            "{mode}: nothing is recorded"
        );
    }
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn an_unbound_prefix_fails_the_request_with_500_after_forwarding() {
    let admin = Admin::start().await;
    assert_forwards_and_fails_twice(&admin, "proxyOnce", "//x:id").await;
}

#[tokio::test]
async fn an_unknown_function_fails_the_same_way() {
    let admin = Admin::start().await;
    assert_forwards_and_fails_twice(&admin, "proxyOnce", "nosuchfn(//a)").await;
}

#[tokio::test]
async fn a_non_xml_body_still_records() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.proxy("proxyOnce", "//x:id").await;
    let stubs = admin.stub_count(port).await;
    // A body that is not XML never runs the selector, in Mountebank too: nothing selected.
    let (status, body) = admin.post(port, "plain").await;
    assert_eq!((status, body.as_str()), (200, "up"));
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    assert_eq!(
        admin.stub_count(port).await,
        stubs + 1,
        "a stub is recorded"
    );
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn proxy_always_and_transparent_fail_the_same_way() {
    let admin = Admin::start().await;
    assert_forwards_and_fails_twice(&admin, "proxyAlways", "//x:id").await;
    // Mountebank runs `predicatesFor` for a transparent proxy too, and discards the result.
    assert_forwards_and_fails_twice(&admin, "proxyTransparent", "//x:id").await;
}

#[tokio::test]
async fn a_transparent_proxy_with_an_evaluable_selector_still_forwards() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.proxy("proxyTransparent", "//id").await;
    let stubs = admin.stub_count(port).await;
    let (status, body) = admin.post(port, "<a><id>1</id></a>").await;
    assert_eq!((status, body.as_str()), (200, "up"));
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    assert_eq!(
        admin.stub_count(port).await,
        stubs,
        "transparent records nothing"
    );
    admin.drop_all(&[port, upstream]).await;
}
