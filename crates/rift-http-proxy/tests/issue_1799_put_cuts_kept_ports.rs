//! Issue #1799: a plain `PUT /imposters` cuts off the requests still being answered on every port it
//! keeps, as Mountebank does: its `PUT` deletes every imposter and recreates them, and the delete
//! destroys each socket before a pending response can be written. Rift keeps the listener and idle
//! connections of an unchanged imposter (the documented "Connections stay open" difference), but a
//! request in a `wait` used to be answered by the reset imposter. `?reconcile=true` exists to keep
//! runtime state, and leaves such a request to finish.
//!
//! Every imposter takes an OS-assigned port.

use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

/// Long enough that the `PUT` lands while the request waits.
const WAIT_MS: u64 = 1500;

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

    async fn post(&self, imposter: &Value) -> u16 {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(imposter)
            .send()
            .await
            .expect("POST");
        let body: Value = response.json().await.expect("json");
        body["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
    }

    async fn wait_for_a_request(&self, port: u16) {
        for _ in 0..200 {
            let imposter: Value = self
                .client
                .get(format!("{}/imposters/{port}", self.url))
                .send()
                .await
                .expect("GET")
                .json()
                .await
                .expect("json");
            if imposter["numberOfRequests"].as_u64() == Some(1) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the request never reached the imposter");
    }

    async fn put(&self, query: &str, imposters: Value) -> Value {
        let response = self
            .client
            .put(format!("{}/imposters{query}", self.url))
            .json(&json!({ "imposters": imposters }))
            .send()
            .await
            .expect("PUT");
        assert_eq!(response.status().as_u16(), 200);
        response.json().await.expect("json")
    }
}

fn waiting(port: Option<u16>) -> Value {
    let mut imposter = json!({
        "protocol": "http",
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "slow" },
                "behaviors": [{ "wait": WAIT_MS }]
            }]
        }]
    });
    if let Some(port) = port {
        imposter["port"] = json!(port);
    }
    imposter
}

/// Start a request on `port`, `PUT` the same imposter back while it waits, and return what the
/// request got along with the `PUT`'s body.
async fn put_during_a_wait(admin: &Admin, query: &str) -> (u16, Result<String, String>, Value) {
    let port = admin.post(&waiting(None)).await;
    let request = tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(10))
            .build()
            .expect("client");
        match client.get(format!("http://127.0.0.1:{port}/")).send().await {
            Ok(response) => response.text().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    });
    // In its `wait` once the imposter has counted it.
    admin.wait_for_a_request(port).await;
    let put = admin.put(query, json!([waiting(Some(port))])).await;
    let answered = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("the request ended")
        .expect("request task");
    (port, answered, put)
}

fn lists(put: &Value, key: &str) -> Vec<u64> {
    put[key]
        .as_array()
        .map(|ports| ports.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_put_cuts_the_requests_a_kept_imposter_is_still_answering() {
    let admin = Admin::start().await;
    let (port, answered, put) = put_during_a_wait(&admin, "").await;

    assert!(
        !lists(&put, "replaced").contains(&u64::from(port))
            && !lists(&put, "deleted").contains(&u64::from(port)),
        "the port was meant to be kept: {put}"
    );
    assert!(
        answered.is_err(),
        "the kept imposter answered a request that was in flight across the PUT: {answered:?}"
    );
    // The listener is kept: the next request is answered.
    let next = reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .expect("answered")
        .text()
        .await
        .expect("body");
    assert_eq!(next, "slow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconciling_put_lets_a_kept_imposters_requests_finish() {
    let admin = Admin::start().await;
    let (_, answered, _) = put_during_a_wait(&admin, "?reconcile=true").await;
    assert_eq!(answered.as_deref(), Ok("slow"));
}
