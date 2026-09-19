//! Issue #1193: `proxyOnce` takes a recording claim before forwarding. The claim was given back
//! only on paths that *return*, so a client that disconnected mid-request — hyper then drops the
//! handler future mid-await — left it held for the life of the imposter: every later identical
//! request was answered `InFlight`, proxied upstream, and never recorded.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

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
        );
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
}

/// One client gives up after 200 ms while the request is still in flight; afterwards three
/// ordinary requests must see a normal `proxyOnce`: the first records, the rest replay.
async fn after_an_abandoned_request(upstream_response: Value, proxy_behaviors: Option<Value>) {
    let admin = Admin::start().await;
    let upstream = free_port();
    admin
        .create(
            json!({"port": upstream, "protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [upstream_response]}]}),
        )
        .await;
    let port = free_port();
    let mut response = json!({"proxy": {
        "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce"
    }});
    if let Some(behaviors) = proxy_behaviors {
        response["_behaviors"] = behaviors;
    }
    admin
        .create(json!({"port": port, "protocol": "http", "stubs": [{"responses": [response]}]}))
        .await;

    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .expect("client");
    let abandoned = impatient
        .get(format!("http://127.0.0.1:{port}/p"))
        .send()
        .await;
    assert!(
        abandoned.is_err(),
        "the first request was meant to time out"
    );
    // Long enough for the abandoned request's upstream call to have finished.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let hits_before = admin.upstream_hits(upstream).await;

    for _ in 0..3 {
        let body = admin
            .client
            .get(format!("http://127.0.0.1:{port}/p"))
            .send()
            .await
            .expect("GET")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "up");
    }
    assert_eq!(
        admin.upstream_hits(upstream).await - hits_before,
        1,
        "only the first follow-up may reach the upstream; the others replay its recording"
    );
    let _ = admin.manager.delete_imposter(port).await;
    let _ = admin.manager.delete_imposter(upstream).await;
}

#[tokio::test]
async fn a_client_that_gives_up_during_the_upstream_call_does_not_leave_the_claim_held() {
    after_an_abandoned_request(
        json!({"is": {"body": "up"}, "_behaviors": {"wait": 1000}}),
        None,
    )
    .await;
}

/// The drop lands inside the proxy response's own `wait` (issue #1189) instead of the forward.
#[tokio::test]
async fn a_client_that_gives_up_during_a_proxy_behavior_does_not_leave_the_claim_held() {
    after_an_abandoned_request(json!({"is": {"body": "up"}}), Some(json!({"wait": 1000}))).await;
}
