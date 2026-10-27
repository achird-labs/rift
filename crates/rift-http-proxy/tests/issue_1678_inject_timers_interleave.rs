//! Issue #1678: a Mountebank-style `inject` or `async` decorate that waits on a timer holds neither
//! its imposter nor a script worker, as in Mountebank, where runs interleave on one event loop.
//!
//! Before, a run held its imposter's state lock for its whole wait, so the imposter's other
//! requests queued behind it, and it slept on a script-pool worker (4-16 of them, shared by every
//! imposter), so a burst of sleeping runs stalled every script in the process.
//!
//! Its own binary, and its tests run one at a time ([`one_at_a_time`]): the script pool is
//! process-wide and the starvation test loads it on purpose.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

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

    /// An imposter with `stubs` and a 20 s script timeout; its port.
    async fn imposter(&self, stubs: Value) -> u16 {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&json!({
                "protocol": "http",
                "stubs": stubs,
                "_rift": {"scriptEngine": {"timeoutMs": 20_000}}
            }))
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

    async fn inject(&self, inject: &str) -> u16 {
        self.imposter(json!([{"responses": [{"inject": inject}]}]))
            .await
    }
}

async fn get(port: u16, path: &str) -> String {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body")
}

/// Send `n` concurrent requests to `port`, and return when all answered, with the bodies.
async fn concurrently(port: u16, n: usize, path: &str) -> Vec<String> {
    let requests: Vec<_> = (0..n)
        .map(|i| {
            let path = path.replace("{i}", &i.to_string());
            tokio::spawn(async move { get(port, &path).await })
        })
        .collect();
    let mut bodies = Vec::new();
    for request in requests {
        bodies.push(request.await.expect("request task"));
    }
    bodies
}

#[tokio::test]
async fn two_timer_injects_on_one_imposter_interleave() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let port = admin
        .inject("config => new Promise(r => setTimeout(() => r({ body: 'x' }), 1000))")
        .await;
    get(port, "/warm-up").await;

    let started = Instant::now();
    let bodies = concurrently(port, 2, "/").await;
    let took = started.elapsed();
    assert_eq!(bodies, ["x", "x"]);
    assert!(
        took < Duration::from_millis(1600),
        "two 1 s waits on one imposter took {took:?}: they ran one after the other"
    );
}

#[tokio::test]
async fn timer_decorate_does_not_serialise_the_imposter() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let port = admin
        .imposter(json!([{"responses": [{
            "is": {"body": "plain"},
            "behaviors": [{"decorate":
                "async config => { await new Promise(r => setTimeout(r, 1000)); \
                 config.response.body = 'decorated'; }"}]
        }]}]))
        .await;
    get(port, "/warm-up").await;

    let started = Instant::now();
    let bodies = concurrently(port, 2, "/").await;
    let took = started.elapsed();
    assert_eq!(bodies, ["decorated", "decorated"]);
    assert!(
        took < Duration::from_millis(1600),
        "two 1 s decorates on one imposter took {took:?}: they ran one after the other"
    );
}

#[tokio::test]
async fn sleeping_injects_do_not_starve_other_imposters() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let quick = admin.inject("config => ({ body: 'quick' })").await;
    assert_eq!(get(quick, "/warm-up").await, "quick");

    // Five sleeping imposters, four requests each: 20 runs waiting 10 s, more than the 16
    // workers the pool can have, spread over imposters as well as within one.
    let mut sleepers = Vec::new();
    for _ in 0..5 {
        let port = admin
            .inject("config => new Promise(r => setTimeout(() => r({ body: 'slept' }), 10000))")
            .await;
        for _ in 0..4 {
            sleepers.push(tokio::spawn(async move { get(port, "/").await }));
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    assert_eq!(get(quick, "/").await, "quick");
    let took = started.elapsed();
    for sleeper in sleepers {
        sleeper.abort();
    }
    assert!(
        took < Duration::from_millis(500),
        "a synchronous inject waited {took:?} behind other imposters' sleeping injects"
    );
}

/// A pin against snapshot write-back: concurrent runs each set their own key after a timer, and
/// every key survives, as in Mountebank's one live `state` object.
#[tokio::test]
async fn concurrent_timer_injects_keep_disjoint_state_updates() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let port = admin
        .inject(
            "config => config.query.read \
               ? { body: Object.keys(config.state).sort().join(',') } \
               : new Promise(r => setTimeout(() => { \
                   config.state['k' + config.query.i] = true; r({ body: 'set' }); }, 300))",
        )
        .await;

    let bodies = concurrently(port, 8, "/?i={i}").await;
    assert!(bodies.iter().all(|b| b == "set"), "{bodies:?}");
    assert_eq!(
        get(port, "/?read=1").await,
        "k0,k1,k2,k3,k4,k5,k6,k7",
        "a concurrent run's key was lost"
    );
}

/// Mountebank hands every run the imposter's one live `state` object: a write is visible to a run
/// that is still waiting, at its next `await`.
#[tokio::test]
async fn a_run_sees_a_concurrent_runs_write_at_its_next_await() {
    let _serial = one_at_a_time().await;
    let admin = Admin::start().await;
    let port = admin
        .inject(
            "async config => { \
               if (config.query.role === 'writer') { config.state.x = 'written'; return { body: 'w' }; } \
               await new Promise(r => setTimeout(r, 600)); \
               return { body: String(config.state.x) }; }",
        )
        .await;
    get(port, "/?role=warm-up").await;

    let reader = tokio::spawn(async move { get(port, "/?role=reader").await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(get(port, "/?role=writer").await, "w");
    assert_eq!(
        reader.await.expect("reader"),
        "written",
        "the waiting run did not see the write made while it waited"
    );
}
