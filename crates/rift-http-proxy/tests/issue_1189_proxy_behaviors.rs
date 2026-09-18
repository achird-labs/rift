//! Issue #1189 (part C of #1184): Mountebank runs a proxy response's behaviors on the upstream
//! response *before* recording it (`proxyAndRecord`: "Run behaviors here to persist decorated
//! response"), so the transformed response is what the client gets and what is recorded. The
//! recorded stub carries only `addWaitBehavior`/`addDecorateBehavior` (`newIsResponse`), never the
//! proxy's own behaviors, so nothing is applied twice. Rift ran no behavior on a proxy response.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        let admin_port = free_port();
        let server = AdminApiServer::new(
            format!("127.0.0.1:{admin_port}").parse().expect("addr"),
            manager.clone(),
            None,
        )
        .with_allow_injection(true);
        tokio::spawn(server.run());
        let url = format!("http://127.0.0.1:{admin_port}");
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client.get(format!("{url}/imposters")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
}

const APPEND_X: &str = "function (request, response) { response.body = response.body + '-X'; }";
const APPEND_Y: &str = "function (request, response) { response.body = response.body + '-Y'; }";
const THROW: &str = "function (request, response) { throw new Error('boom'); }";

fn proxy_imposter(port: u16, proxy: Value, behaviors: Value) -> Value {
    json!({"port": port, "protocol": "http", "stubs": [{"responses": [
        {"proxy": proxy, "_behaviors": behaviors}
    ]}]})
}

#[tokio::test]
async fn decorate_rewrites_the_live_proxied_response() {
    let admin = Admin::start().await;
    let upstream = admin
        .upstream(json!({"is": {"headers": {"x-up": "1"}, "body": "up"}}))
        .await;
    let port = free_port();
    let created = admin
        .create(proxy_imposter(
            port,
            json!({"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"}),
            json!({"decorate": APPEND_X}),
        ))
        .await;
    let response = admin.get(port).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["x-up"], "1");
    assert_eq!(response.headers()["x-rift-proxy"], "true");
    // A body longer than the upstream's: a stale upstream `content-length` would truncate it.
    assert_eq!(response.text().await.expect("body"), "up-X");
    assert!(
        created["_rift"]["warnings"]
            .as_array()
            .is_none_or(|w| w.iter().all(|w| w["warningType"] != "config_key_ignored")),
        "{created}"
    );
    admin.drop_all(&[port, upstream]).await;
}

/// `proxyOnce` without generators replays from the proxy store, not from a stub. That door must
/// serve what was recorded — already decorated — and run nothing on it again.
#[tokio::test]
async fn a_proxy_once_replay_is_decorated_exactly_once() {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin
        .create(proxy_imposter(
            port,
            json!({"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce"}),
            json!({"decorate": APPEND_X}),
        ))
        .await;
    let first = admin.get(port).await.text().await.expect("body");
    let second = admin.get(port).await.text().await.expect("body");
    assert_eq!(first, "up-X");
    assert_eq!(second, "up-X", "the replay was decorated again");
    assert_eq!(
        admin.upstream_hits(upstream).await,
        1,
        "the second request was not a replay"
    );
    admin.drop_all(&[port, upstream]).await;
}

/// Mountebank's `newIsResponse`: the recorded stub holds the transformed body and only the
/// `addDecorateBehavior` script, so a replay is `Y(X(upstream))`.
#[tokio::test]
async fn the_recorded_stub_holds_the_decorated_body_and_only_add_decorate_behavior() {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin
        .create(proxy_imposter(
            port,
            json!({
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce",
                "predicateGenerators": [{"matches": {"path": true}}],
                "addDecorateBehavior": APPEND_Y
            }),
            json!({"decorate": APPEND_X}),
        ))
        .await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "up-X");

    let stubs = admin.imposter(port).await["stubs"].clone();
    let recorded = &stubs[0]["responses"][0];
    assert_eq!(recorded["is"]["body"], "up-X", "{stubs}");
    assert_eq!(
        recorded["behaviors"],
        json!([{"decorate": APPEND_Y}]),
        "{stubs}"
    );

    assert_eq!(admin.get(port).await.text().await.expect("body"), "up-X-Y");
    assert_eq!(admin.upstream_hits(upstream).await, 1);
    admin.drop_all(&[port, upstream]).await;
}

/// `addWaitBehavior` records how long the upstream took. A `wait` behavior runs after that is
/// measured, so it delays the live response without being baked into the recording.
#[tokio::test]
async fn a_wait_delays_the_response_but_not_the_recorded_latency() {
    let admin = Admin::start().await;
    let upstream = admin
        .upstream(json!({"is": {"body": "up"}, "_behaviors": {"wait": 200}}))
        .await;
    let port = free_port();
    admin
        .create(proxy_imposter(
            port,
            json!({
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce",
                "predicateGenerators": [{"matches": {"path": true}}],
                "addWaitBehavior": true
            }),
            json!({"wait": 600}),
        ))
        .await;
    let start = Instant::now();
    assert_eq!(admin.get(port).await.text().await.expect("body"), "up");
    assert!(
        start.elapsed() >= Duration::from_millis(800),
        "served after {:?}",
        start.elapsed()
    );
    let stubs = admin.imposter(port).await["stubs"].clone();
    let recorded_wait = stubs[0]["responses"][0]["behaviors"][0]["wait"]
        .as_u64()
        .unwrap_or_else(|| panic!("no recorded wait: {stubs}"));
    assert!(
        (200..600).contains(&recorded_wait),
        "recorded wait {recorded_wait}ms is not the upstream's latency"
    );
    admin.drop_all(&[port, upstream]).await;
}

/// A behavior that failed produced a response the configuration did not ask for. Recording it would
/// replay that forever, so nothing is recorded and the next request forwards again.
#[tokio::test]
async fn a_failed_behavior_records_nothing_lenient_or_strict() {
    let admin = Admin::start().await;
    for strict in [false, true] {
        let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
        let port = free_port();
        let mut imposter = proxy_imposter(
            port,
            json!({
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce",
                "predicateGenerators": [{"matches": {"path": true}}]
            }),
            json!({"decorate": THROW}),
        );
        imposter["strictBehaviors"] = json!(strict);
        admin.create(imposter).await;

        let first = admin.get(port).await;
        assert_eq!(
            first.headers()["x-rift-decorate-error"],
            "true",
            "strict={strict}"
        );
        if strict {
            assert_eq!(first.status().as_u16(), 500);
        } else {
            assert_eq!(first.status().as_u16(), 200);
            assert_eq!(first.text().await.expect("body"), "up");
        }
        let stubs = admin.imposter(port).await["stubs"].clone();
        assert_eq!(
            stubs.as_array().map(Vec::len),
            Some(1),
            "strict={strict}: a stub was recorded: {stubs}"
        );

        let second = admin.get(port).await;
        assert_eq!(
            second.headers()["x-rift-decorate-error"],
            "true",
            "strict={strict}"
        );
        assert_eq!(admin.upstream_hits(upstream).await, 2, "strict={strict}");
        admin.drop_all(&[port, upstream]).await;
    }
}

/// The same for the store-replay door: with no generators a failed behavior must not be stored.
#[tokio::test]
async fn a_failed_behavior_is_not_stored_for_a_proxy_once_replay() {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin
        .create(proxy_imposter(
            port,
            json!({"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce"}),
            json!({"decorate": THROW}),
        ))
        .await;
    admin.get(port).await;
    admin.get(port).await;
    assert_eq!(admin.upstream_hits(upstream).await, 2);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_binary_upstream_body_survives_the_pipeline_byte_for_byte() {
    let admin = Admin::start().await;
    // 0xff 0x00 0xfe 0x41 — not UTF-8.
    let upstream = admin
        .upstream(json!({"is": {"body": "/wD+QQ==", "_mode": "binary"}}))
        .await;
    let port = free_port();
    admin
        .create(proxy_imposter(
            port,
            json!({"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"}),
            json!({"wait": 1}),
        ))
        .await;
    let response = admin.get(port).await;
    assert!(response.headers().get("x-rift-binary-error").is_none());
    let bytes = response.bytes().await.expect("body");
    assert_eq!(bytes.as_ref(), &[0xff, 0x00, 0xfe, 0x41]);
    admin.drop_all(&[port, upstream]).await;
}

#[tokio::test]
async fn a_proxy_response_without_behaviors_is_unchanged() {
    let admin = Admin::start().await;
    let upstream = admin
        .upstream(json!({"is": {"headers": {"x-up": "1"}, "body": "up"}}))
        .await;
    let port = free_port();
    admin
        .create(json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"proxy": {"to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent"}}
        ]}]}))
        .await;
    let response = admin.get(port).await;
    assert_eq!(response.headers()["x-up"], "1");
    assert_eq!(response.headers()["content-length"], "2");
    assert_eq!(response.text().await.expect("body"), "up");
    admin.drop_all(&[port, upstream]).await;
}

/// A block that only sets `repeat` transforms nothing, so the upstream response is relayed and
/// recorded exactly as it came — its `content-length` included.
#[tokio::test]
async fn a_repeat_only_block_leaves_the_proxied_response_untouched() {
    let admin = Admin::start().await;
    let upstream = admin.upstream(json!({"is": {"body": "up"}})).await;
    let port = free_port();
    admin
        .create(
            json!({"port": port, "protocol": "http", "stubs": [{"responses": [
                {"proxy": {
                    "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce",
                    "predicateGenerators": [{"matches": {"path": true}}]
                }, "_behaviors": {"repeat": 2}}
            ]}]}),
        )
        .await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "up");
    let stubs = admin.imposter(port).await["stubs"].clone();
    let headers = stubs[0]["responses"][0]["is"]["headers"].clone();
    let has_length = headers
        .as_object()
        .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("content-length")));
    assert!(
        has_length,
        "the recorded response lost its content-length: {stubs}"
    );
    admin.drop_all(&[port, upstream]).await;
}

#[test]
fn the_linter_no_longer_reports_a_proxy_behaviors_block() {
    let imposter = json!({"port": 4545, "protocol": "http", "stubs": [{"responses": [
        {"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": {"wait": 1}}
    ]}]});
    let lint = rift_lint::lint_json(
        &imposter.to_string(),
        "fixture.json",
        &rift_lint::LintOptions::default(),
    );
    assert!(
        lint.issues.iter().all(|i| i.code != "W017"),
        "{:?}",
        lint.issues
    );
}
