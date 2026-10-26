//! Issue #1643: `DELETE /imposters/:port` cuts off every request the imposter is still answering,
//! not only one in a `wait` (#1616): a slow proxy upstream, a running `shellTransform`, an inject
//! waiting on a timer.
//!
//! Mountebank destroys every open socket of an imposter it deletes (`baseHttpServer.js` `close`),
//! so none of these requests is ever answered. Rift's delete held for its 5 s drain and the client
//! was later answered by the deleted imposter when the slow step ended.
//!
//! Every slow step here lasts at least 5 s, and every assertion is bounded at 1-2 s. Every imposter
//! and upstream takes an OS-assigned port, so none of these tests owns a fixed port.

use std::sync::Arc;
use std::time::{Duration, Instant};

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
        let manager = Arc::new(ImposterManager::new());
        let running = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
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

    /// Create an imposter with `stubs`, returning its assigned port.
    async fn imposter(&self, extra: Value) -> u16 {
        let mut body = json!({"protocol": "http"});
        if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
            body.extend(extra);
        }
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&body)
            .send()
            .await
            .expect("POST");
        let status = response.status().as_u16();
        let body: Value = response.json().await.expect("json");
        assert_eq!(status, 201, "{body}");
        body["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
    }

    /// `DELETE /imposters/:port`, returning how long it took.
    async fn delete(&self, port: u16) -> Duration {
        let started = Instant::now();
        let response = self
            .client
            .delete(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(response.status().as_u16(), 200);
        started.elapsed()
    }
}

/// An upstream that accepts connections and never answers, held open until the test ends.
async fn silent_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    port
}

async fn proxy_to_silent_upstream(admin: &Admin) -> u16 {
    let upstream = silent_upstream().await;
    admin
        .imposter(json!({"stubs": [{"responses": [
            {"proxy": {"to": format!("http://127.0.0.1:{upstream}")}}
        ]}]}))
        .await
}

/// Send `GET url` in the background, delete `port` once the request is in its slow step, and
/// assert both the delete and the request end within a second, the request without a response.
async fn assert_delete_cuts_off(admin: &Admin, port: u16, url: String) {
    let client = reqwest::Client::builder()
        .http1_only()
        .build()
        .expect("client");
    let request = tokio::spawn(async move {
        let started = Instant::now();
        let result = client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        (result.map(|r| r.status().as_u16()), started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let took = admin.delete(port).await;
    assert!(took < Duration::from_secs(1), "DELETE took {took:?}");

    let (result, after) = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .expect("the request ends with the delete, not after the slow step")
        .expect("request task");
    assert!(
        result.is_err(),
        "the request gets no response from the deleted imposter, got {result:?}"
    );
    assert!(
        after < Duration::from_secs(2),
        "request ended after {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cuts_off_a_request_waiting_on_a_slow_proxy_upstream() {
    let admin = Admin::start().await;
    let port = proxy_to_silent_upstream(&admin).await;
    assert_delete_cuts_off(&admin, port, format!("http://127.0.0.1:{port}/")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cuts_off_a_running_shell_transform() {
    let admin = Admin::start().await;
    // The child outlives the delete; its output is discarded, as Mountebank never delivers it.
    let port = admin
        .imposter(json!({"stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "ok"},
            "_behaviors": {"shellTransform": ["sleep 5; printf '{}'; true"]}
        }]}]}))
        .await;
    assert_delete_cuts_off(&admin, port, format!("http://127.0.0.1:{port}/")).await;
}

#[cfg(feature = "javascript")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cuts_off_a_slow_inject() {
    let admin = Admin::start().await;
    let port = admin
        .imposter(json!({
            "_rift": {"scriptEngine": {"timeoutMs": 10000}},
            "stubs": [{"responses": [{
                "inject": "function (config) { return new Promise(r => setTimeout(() => r({statusCode: 200, body: 'late'}), 5000)); }"
            }]}]
        }))
        .await;
    assert_delete_cuts_off(&admin, port, format!("http://127.0.0.1:{port}/")).await;
}

// The `/__rift/:port/` gateway dispatches into the imposter in-process; the abort reaches a slow
// step reached through it as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cuts_off_a_slow_proxy_reached_through_the_gateway() {
    let admin = Admin::start().await;
    let port = proxy_to_silent_upstream(&admin).await;
    let url = format!("{}/__rift/{port}/", admin.url);
    assert_delete_cuts_off(&admin, port, url).await;
}
