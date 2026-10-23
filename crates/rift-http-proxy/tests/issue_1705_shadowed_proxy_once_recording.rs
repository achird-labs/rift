//! Issue #1705: a `proxyOnce` request that reaches a proxy stub missed every recording in front of
//! it, so it is forwarded and recorded in front of that stub, as Mountebank's `proxyAndRecord`
//! does. Before, the proxy store's port-wide claim answered `AlreadyRecorded` for an identity
//! recorded in front of *another* proxy stub (or moved behind this one), and Rift replayed that
//! recording without forwarding.
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
        // Injection on for the guard's inject generator; no other test uses a script.
        let server =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .with_allow_injection(true);
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
        let status = response.status().as_u16();
        let body: Value = response.json().await.expect("json");
        assert_eq!(status, 201, "{imposter} answered {body}");
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

    /// What each stub is, in order: the recorded body of an `is` stub, or `proxy`.
    async fn layout(&self, port: u16) -> Vec<String> {
        self.stubs(port)
            .await
            .iter()
            .map(|stub| {
                let response = &stub["responses"][0];
                if response.get("proxy").is_some() {
                    "proxy".to_string()
                } else {
                    response["is"]["body"].as_str().expect("a body").to_string()
                }
            })
            .collect()
    }

    async fn upstream_hits(&self, upstream: u16) -> usize {
        self.manager
            .get_imposter(upstream)
            .expect("upstream")
            .get_recorded_requests()
            .len()
    }

    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.url));
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request.send().await.expect("admin request");
        let status = response.status().as_u16();
        assert!(
            (200..300).contains(&status),
            "{method} {path} answered {status}: {}",
            response.text().await.unwrap_or_default()
        );
    }

    /// An upstream recording its requests and answering `up/b#0`, `#1`, `#2` in turn on `/b`, and
    /// the proxy stub that forwards to it.
    async fn upstream(&self) -> (u16, Value) {
        let upstream = self
            .create(
                json!({"protocol": "http", "recordRequests": true, "stubs": [{
                    "predicates": [{"equals": {"path": "/b"}}],
                    "responses": [
                        {"is": {"body": "up/b#0"}},
                        {"is": {"body": "up/b#1"}},
                        {"is": {"body": "up/b#2"}}
                    ]
                }]}),
            )
            .await;
        let proxy = json!({"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{"matches": {"path": true}}]
        }}]});
        (upstream, proxy)
    }

    async fn get_b(&self, port: u16) -> String {
        self.client
            .get(format!("http://127.0.0.1:{port}/b"))
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("body")
    }

    /// The shrunk case: `/b` recorded in front of proxy₁, then a second identical proxy stub put
    /// at index 0 (a POST of a plain stub there, then a PUT replacing it), then `/b` again.
    async fn second_proxy(&self) -> (u16, u16) {
        let (upstream, proxy) = self.upstream().await;
        let port = self
            .create(json!({"protocol": "http", "stubs": [proxy.clone()]}))
            .await;
        assert_eq!(self.get_b(port).await, "up/b#0");
        assert_eq!(self.layout(port).await, ["up/b#0", "proxy"]);

        self.admin(
            reqwest::Method::POST,
            &format!("/imposters/{port}/stubs"),
            Some(json!({"index": 0, "stub": {"responses": [{"is": {"body": "p0"}}]}})),
        )
        .await;
        self.admin(
            reqwest::Method::PUT,
            &format!("/imposters/{port}/stubs/0"),
            Some(proxy),
        )
        .await;
        assert_eq!(self.layout(port).await, ["proxy", "up/b#0", "proxy"]);
        (port, upstream)
    }
}

#[tokio::test]
async fn a_second_proxy_once_stub_forwards_an_identity_recorded_in_front_of_another_proxy() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.second_proxy().await;

    assert_eq!(admin.get_b(port).await, "up/b#1", "forwarded, not replayed");
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(
        admin.layout(port).await,
        ["up/b#1", "proxy", "up/b#0", "proxy"],
        "recorded in front of the proxy stub that answered"
    );
    // The new recording answers the next request.
    assert_eq!(admin.get_b(port).await, "up/b#1");
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.manager.delete_all().await;
}

#[tokio::test]
async fn a_recording_moved_behind_its_proxy_is_forwarded_and_recorded_again() {
    let admin = Admin::start().await;
    let (upstream, proxy) = admin.upstream().await;
    let port = admin
        .create(json!({"protocol": "http", "stubs": [proxy]}))
        .await;
    assert_eq!(admin.get_b(port).await, "up/b#0");

    // A move keeps the recording's state, and with it its claim (a `PUT .../stubs` makes every
    // state new, so the claim is forgotten there, #1386).
    admin.manager.move_stub(port, 0, 1).await.expect("move");
    assert_eq!(admin.layout(port).await, ["proxy", "up/b#0"]);

    assert_eq!(admin.get_b(port).await, "up/b#1", "forwarded, not replayed");
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    assert_eq!(admin.layout(port).await, ["up/b#1", "proxy", "up/b#0"]);
    admin.manager.delete_all().await;
}

// Two stubs carry the claim once the shadowed identity is recorded again. Deleting the front one
// leaves the claim held by the one behind, so the identity is shadowed again and re-forwarded:
// neither a replay of `#0` nor a 500.
#[tokio::test]
async fn deleting_the_new_recording_keeps_the_claim_and_reforwards() {
    let admin = Admin::start().await;
    let (port, upstream) = admin.second_proxy().await;
    assert_eq!(admin.get_b(port).await, "up/b#1");

    admin
        .admin(
            reqwest::Method::DELETE,
            &format!("/imposters/{port}/stubs/0"),
            None,
        )
        .await;
    assert_eq!(admin.layout(port).await, ["proxy", "up/b#0", "proxy"]);

    assert_eq!(admin.get_b(port).await, "up/b#2");
    assert_eq!(admin.upstream_hits(upstream).await, 3);
    assert_eq!(
        admin.layout(port).await,
        ["up/b#2", "proxy", "up/b#0", "proxy"]
    );
    admin.manager.delete_all().await;
}

// Guard (passes before and after #1705): a recording in front of the answering proxy stub is the
// concurrent-first-request case, where a request matched a snapshot taken before the winner's
// stub landed, so it still replays without an upstream call. An inject generator whose predicate
// never matches its own request reaches that arm deterministically.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn an_identity_recorded_in_front_of_the_answering_stub_still_replays() {
    let admin = Admin::start().await;
    let (upstream, _) = admin.upstream().await;
    let port = admin
        .create(
            json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
                "to": format!("http://127.0.0.1:{upstream}"),
                "mode": "proxyOnce",
                "predicateGenerators": [{
                    "inject": "function (config) { return [{ equals: { path: '/never' } }]; }"
                }]
            }}]}]}),
        )
        .await;

    assert_eq!(admin.get_b(port).await, "up/b#0");
    assert_eq!(admin.layout(port).await, ["up/b#0", "proxy"]);

    assert_eq!(admin.get_b(port).await, "up/b#0", "replayed from the store");
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    assert_eq!(admin.layout(port).await, ["up/b#0", "proxy"]);
    admin.manager.delete_all().await;
}
