//! Issue #1616 (part B): `DELETE /imposters/:port` cuts off requests still in a `wait`.
//!
//! Mountebank destroys every open socket of an imposter it deletes (`baseHttpServer.js` `close`),
//! so a client in a `wait` sees the connection close at once. Rift's delete waited out its 5 s
//! drain, and the client was later answered `200` by the deleted imposter when the wait ended.
//!
//! The waits here are a minute long and never sat through: every assertion is bounded at 1-2 s.
//! Every imposter takes an engine-assigned port, so none of these tests owns a fixed port.

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
            .bind()
            .await
            .expect("admin API binds");
        Admin {
            url: format!("http://{}", running.local_addr()),
            client: reqwest::Client::new(),
            _running: running,
        }
    }

    /// An imposter whose one stub answers `200 ok` after a minute.
    async fn long_wait_imposter(&self) -> u16 {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&json!({"protocol": "http", "stubs": [{"responses": [
                {"is": {"statusCode": 200, "body": "ok"}, "_behaviors": {"wait": 60000}}
            ]}]}))
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201);
        let body: Value = response.json().await.expect("json");
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

/// Send `GET url` on `client` in the background, delete `port` once the request is waiting, and
/// assert both the delete and the request end within a second, the request without a response.
async fn assert_delete_cuts_off(admin: &Admin, port: u16, client: reqwest::Client, url: String) {
    let request = tokio::spawn(async move {
        let started = Instant::now();
        let result = client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        (result.map(|r| r.status().as_u16()), started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let took = admin.delete(port).await;
    assert!(took < Duration::from_secs(1), "DELETE took {took:?}");

    let (result, after) = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .expect("the request ends with the delete, not after the wait")
        .expect("request task");
    assert!(
        result.is_err(),
        "the waiting request gets no response from the deleted imposter, got {result:?}"
    );
    assert!(
        after < Duration::from_secs(2),
        "request ended after {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_closes_an_http1_connection_held_by_a_wait() {
    let admin = Admin::start().await;
    let port = admin.long_wait_imposter().await;
    let client = reqwest::Client::builder()
        .http1_only()
        .build()
        .expect("client");
    assert_delete_cuts_off(&admin, port, client, format!("http://127.0.0.1:{port}/")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_closes_an_http2_connection_held_by_a_wait() {
    let admin = Admin::start().await;
    let port = admin.long_wait_imposter().await;
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("client");
    assert_delete_cuts_off(&admin, port, client, format!("http://127.0.0.1:{port}/")).await;
}

// The `/__rift/:port/` gateway dispatches into the imposter in-process, so the abort reaches a
// request that came in through the admin port too: the carrier closes that connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cuts_off_a_wait_reached_through_the_gateway() {
    let admin = Admin::start().await;
    let port = admin.long_wait_imposter().await;
    let url = format!("{}/__rift/{port}/", admin.url);
    let client = reqwest::Client::builder()
        .http1_only()
        .build()
        .expect("client");
    assert_delete_cuts_off(&admin, port, client, url).await;
}
