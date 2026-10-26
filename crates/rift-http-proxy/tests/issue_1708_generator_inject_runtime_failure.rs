//! Issue #1708: a proxy `predicateGenerators` `inject` that throws, or returns something that is not
//! an array, when a real request reaches it fails that request with Mountebank's `500 invalid
//! predicateGenerator injection`, after forwarding it and running its behaviors, in every proxy
//! mode. Rift used to serve the upstream response with `x-rift-generator-error`; under
//! `proxyTransparent` it did not run the inject at all.
//!
//! An invalid generated predicate is different: Mountebank serves the upstream then and records a
//! stub that fails every later request, so Rift keeps serving the upstream with the header and
//! records nothing (documented in `differences.md`).
//!
//! Every inject is guarded on `config.request.isDryRun`, so the create-time dry run (#1580) admits
//! it. Every imposter takes an engine-assigned port.

use std::sync::Arc;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    url: String,
    client: reqwest::Client,
    _running: RunningAdminApi,
}

impl Admin {
    async fn start() -> Self {
        let running = AdminApiServer::new(
            "127.0.0.1:0".parse().expect("addr"),
            Arc::new(ImposterManager::new()),
            None,
        )
        .with_allow_injection(true)
        .bind()
        .await
        .expect("admin API binds");
        Admin {
            url: format!("http://{}", running.local_addr()),
            client: reqwest::Client::new(),
            _running: running,
        }
    }

    async fn create(&self, imposter: Value) -> u16 {
        let reply = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        let status = reply.status().as_u16();
        let body: Value = reply.json().await.expect("json");
        assert_eq!(status, 201, "{body}");
        body["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
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

    /// An upstream that answers `200 upstream` and counts its requests.
    async fn upstream(&self) -> u16 {
        self.create(json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "upstream"}}]}]}))
            .await
    }

    async fn upstream_hits(&self, port: u16) -> u64 {
        self.imposter(port).await["numberOfRequests"]
            .as_u64()
            .expect("numberOfRequests")
    }

    /// A proxy to `upstream` in `mode` whose one generator is `inject`.
    async fn proxy(&self, upstream: u16, mode: &str, inject: &str) -> u16 {
        let response = json!({"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": mode,
            "predicateGenerators": [{"inject": inject}]
        }});
        self.create(json!({"protocol": "http", "stubs": [{"responses": [response]}]}))
            .await
    }

    async fn stub_count(&self, port: u16) -> usize {
        self.imposter(port).await["stubs"]
            .as_array()
            .map_or(0, Vec::len)
    }
}

/// `body` guarded so the dry run admits it: it runs only on a real request.
fn guarded(body: &str) -> String {
    format!("function (config) {{ if (config.request.isDryRun) {{ return []; }} {body} }}")
}

async fn send(port: u16) -> (u16, Option<String>, Value) {
    let reply = reqwest::get(format!("http://127.0.0.1:{port}/thing"))
        .await
        .expect("request");
    let status = reply.status().as_u16();
    let marker = reply
        .headers()
        .get("x-rift-generator-error")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let text = reply.text().await.expect("body");
    (
        status,
        marker,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn a_throwing_generator_inject_fails_with_500_after_forwarding() {
    let admin = Admin::start().await;
    let inject = guarded("throw new Error('boom');");
    for mode in ["proxyOnce", "proxyAlways", "proxyTransparent"] {
        let upstream = admin.upstream().await;
        let proxy = admin.proxy(upstream, mode, &inject).await;
        for _ in 0..2 {
            let (status, marker, body) = send(proxy).await;
            assert_eq!(status, 500, "{mode}: {body}");
            let error = &body["errors"][0];
            assert_eq!(error["code"], "invalid injection", "{mode}: {body}");
            assert_eq!(
                error["message"], "invalid predicateGenerator injection",
                "{mode}"
            );
            assert_eq!(error["data"], "boom", "{mode}: {body}");
            assert_eq!(error["source"], format!("({inject})(config);"), "{mode}");
            assert_eq!(marker.as_deref(), Some("script-error"), "{mode}");
        }
        assert_eq!(
            admin.upstream_hits(upstream).await,
            2,
            "{mode}: forwarded each time"
        );
        assert_eq!(admin.stub_count(proxy).await, 1, "{mode}: nothing recorded");
    }
}

#[tokio::test]
async fn a_generator_inject_that_throws_a_value_without_message_omits_data() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    let proxy = admin
        .proxy(upstream, "proxyOnce", &guarded("throw 'text';"))
        .await;
    let (status, _, body) = send(proxy).await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(
        body["errors"][0]["message"],
        "invalid predicateGenerator injection"
    );
    assert!(body["errors"][0].get("data").is_none(), "{body}");
}

#[tokio::test]
async fn a_generator_inject_returning_a_non_array_fails_with_500() {
    let admin = Admin::start().await;
    for returned in ["return { a: 1 };", "return undefined;", "return 'ab';"] {
        let upstream = admin.upstream().await;
        let proxy = admin.proxy(upstream, "proxyOnce", &guarded(returned)).await;
        let (status, _, body) = send(proxy).await;
        assert_eq!(status, 500, "{returned}: {body}");
        assert_eq!(
            body["errors"][0]["message"],
            "invalid predicateGenerator injection"
        );
        assert!(body["errors"][0]["data"].is_string(), "{returned}: {body}");
        assert_eq!(admin.upstream_hits(upstream).await, 1, "{returned}");
        assert_eq!(admin.stub_count(proxy).await, 1, "{returned}");
    }
}

// Mountebank serves the upstream here and records a stub that fails every later request; Rift
// serves the upstream, marks it and records nothing.
#[tokio::test]
async fn an_invalid_generated_predicate_still_serves_the_upstream() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    let proxy = admin
        .proxy(
            upstream,
            "proxyOnce",
            &guarded("return [{ equal: { path: '/thing' } }];"),
        )
        .await;
    let (status, marker, body) = send(proxy).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, Value::String("upstream".to_string()));
    assert_eq!(marker.as_deref(), Some("invalid-predicate"));
    assert_eq!(admin.stub_count(proxy).await, 1);
}

// The proxy's own behaviors run before the recording, as in Mountebank: a behavior that fails
// under strictBehaviors is the answer, not the generator's failure.
#[tokio::test]
async fn a_failing_behavior_still_wins_over_a_generator_failure() {
    let admin = Admin::start().await;
    let upstream = admin.upstream().await;
    let proxy = admin
        .create(
            json!({"protocol": "http", "strictBehaviors": true, "stubs": [{"responses": [{
                "proxy": {
                    "to": format!("http://127.0.0.1:{upstream}"),
                    "mode": "proxyOnce",
                    "predicateGenerators": [{"inject": guarded("throw new Error('boom');")}]
                },
                "_behaviors": {"shellTransform": "exit 3"}
            }]}]}),
        )
        .await;
    let (status, _, body) = send(proxy).await;
    assert_eq!(status, 500, "{body}");
    assert_ne!(
        body["errors"][0]["message"], "invalid predicateGenerator injection",
        "the behavior's failure answers first: {body}"
    );
}
