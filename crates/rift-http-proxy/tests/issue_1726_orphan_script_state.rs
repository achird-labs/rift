//! Issue #1726: a script run that outlives its imposter's delete must not seed the script `state`
//! of the imposter re-created on the same port.
//!
//! Script state was a process-wide map keyed by port, cleared on delete and re-created on the next
//! lookup. A job already handed to the script pool when the delete cut the request off (#1643), or
//! a predicate scan still running on its matcher thread, looked the port up after the clear and
//! wrote into the entry the re-created imposter then read. In Mountebank `imposterState` belongs to
//! the imposter object, so a re-created imposter always starts with `{}`.
//!
//! Its own binary, and its tests run one at a time ([`one_at_a_time`]): the script pool is
//! process-wide, the first three tests fill it on purpose, and a test whose jobs queue behind
//! another's blockers would no longer order its writes before its read.
//! Every imposter takes an engine-assigned port; a re-create reuses the port its delete just freed.

use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

/// Writes `leak` into the script state of whatever runs it.
const LEAK_INJECT: &str =
    "function (config) { config.state.leak = 'old'; return { body: 'victim' }; }";
const LEAK_DECORATE: &str = "function (config) { config.state.leak = 'old'; }";
const LEAK_PREDICATE: &str = "function (config) { config.state.leak = 'old'; return true; }";
/// Answers with the script state it sees.
const READ_STATE: &str = "function (config) { return { body: JSON.stringify(config.state) }; }";

/// Serializes this binary's tests over the shared script pool.
async fn one_at_a_time() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

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

    /// Create an imposter from `body` (protocol and script timeout added), returning its port.
    async fn imposter(&self, mut body: Value) -> u16 {
        if let Some(object) = body.as_object_mut() {
            object.insert("protocol".into(), json!("http"));
            object.insert(
                "_rift".into(),
                json!({"scriptEngine": {"timeoutMs": 20_000}}),
            );
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

    async fn delete(&self, port: u16) {
        let response = self
            .client
            .delete(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(response.status().as_u16(), 200);
    }

    /// Re-create `port` with one stub that answers with its script state.
    async fn recreate_reader(&self, port: u16) {
        let created = self
            .imposter(json!({"port": port, "stubs": [{"responses": [{"inject": READ_STATE}]}]}))
            .await;
        assert_eq!(created, port);
    }
}

async fn get(port: u16) -> reqwest::Result<String> {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/"))
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .text()
        .await
}

/// Occupy every script-pool worker (4-16) with an inject that keeps it busy for 1.5 s, so a job
/// submitted next waits in a queue. Busy, not sleeping: a run waiting on a timer holds no worker
/// (issue #1678). Sixteen imposters, one run each, as the pool keeps each imposter's runs on one
/// worker and spreads imposters over the workers.
async fn fill_the_pool(admin: &Admin) -> Vec<tokio::task::JoinHandle<()>> {
    let mut blockers = Vec::new();
    for _ in 0..16 {
        let blocker = admin
            .imposter(json!({"stubs": [{"responses": [{"inject":
                "function (config) { var e = Date.now() + 1500; while (Date.now() < e) {} \
                 return { body: 'b' }; }"
            }]}]}))
            .await;
        blockers.push(tokio::spawn(async move {
            let _ = get(blocker).await;
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    blockers
}

/// Queue a run of the victim imposter `victim` behind a full pool, delete the victim while its run
/// waits, re-create its port, and return the state the new imposter starts with.
async fn state_after_a_queued_run_outlives_the_delete(admin: &Admin, victim: Value) -> String {
    let blockers = fill_the_pool(admin).await;
    let port = admin.imposter(victim).await;
    let request = tokio::spawn(get(port));
    tokio::time::sleep(Duration::from_millis(200)).await;

    admin.delete(port).await;
    admin.recreate_reader(port).await;
    for blocker in blockers {
        blocker.await.expect("blocker task");
    }
    // The cut-off request; its queued run has had its turn once the blockers are done.
    let _ = request.await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    get(port).await.expect("the re-created imposter answers")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_imposters_queued_inject_does_not_seed_the_recreated_state() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let state = state_after_a_queued_run_outlives_the_delete(
        &admin,
        json!({"stubs": [{"responses": [{"inject": LEAK_INJECT}]}]}),
    )
    .await;
    assert_eq!(state, "{}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_imposters_queued_decorate_does_not_seed_the_recreated_state() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let state = state_after_a_queued_run_outlives_the_delete(
        &admin,
        json!({"stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "victim"},
            "_behaviors": {"decorate": LEAK_DECORATE}
        }]}]}),
    )
    .await;
    assert_eq!(state, "{}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_imposters_predicate_inject_does_not_seed_the_recreated_state() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let state = state_after_a_queued_run_outlives_the_delete(
        &admin,
        json!({"stubs": [{
            "predicates": [{"inject": LEAK_PREDICATE}],
            "responses": [{"is": {"statusCode": 200, "body": "victim"}}]
        }]}),
    )
    .await;
    assert_eq!(state, "{}");
}

// No busy pool: the matcher thread is not cancelled by the delete, so a scan that is still running
// submits the next stub's predicate inject after the delete has gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn predicate_scan_running_past_delete_does_not_seed_the_recreated_state() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let port = admin
        .imposter(json!({"stubs": [
            {"predicates": [{"inject":
                "function (config) { var end = Date.now() + 500; while (Date.now() < end) {} return false; }"
            }], "responses": [{"is": {"body": "slow"}}]},
            {"predicates": [{"inject":
                "function (config) { config.state.leak = 'old'; return false; }"
            }], "responses": [{"is": {"body": "leak"}}]}
        ]}))
        .await;
    let request = tokio::spawn(get(port));
    tokio::time::sleep(Duration::from_millis(200)).await;

    admin.delete(port).await;
    admin.recreate_reader(port).await;
    let _ = request.await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(get(port).await.expect("re-created answers"), "{}");
}
