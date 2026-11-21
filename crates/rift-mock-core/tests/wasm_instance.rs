//! Issue #1793: `_rift.script.instance: "imposter"` keeps one wasm instance per component,
//! `export` and `knobs`, so guest memory is state; it survives stub edits that keep it referenced.
//!
//! The component is `memory_counter`: each call adds one to a wasm global and answers
//! `delay(<count>)`, which the imposter serves as `x-rift-latency-ms: <count>`.
#![cfg(feature = "wasm")]

#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use rift_mock_core::imposter::{ImposterConfig, ImposterManager, Stub};
use serde_json::{Value, json};
use std::time::Duration;

fn twin(tag: &str, knobs: Value) -> Value {
    json!({ "responses": [{ "_rift": { "script": {
        "engine": "wasm",
        "code": wasm_fixtures::b64(&wasm_fixtures::memory_counter(tag)),
        "instance": "imposter",
        "knobs": knobs,
    } } }] })
}

fn sibling(body: &str) -> Value {
    json!({ "predicates": [{ "equals": { "path": "/sibling" } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }] })
}

fn outage() -> Value {
    json!({ "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [{ "is": { "statusCode": 503 } }] })
}

fn stub(value: Value) -> Stub {
    serde_json::from_value(value).expect("stub")
}

fn config(port: Option<u16>, stubs: Vec<Value>, engine: Value) -> ImposterConfig {
    let mut doc =
        json!({ "protocol": "http", "_rift": { "scriptEngine": engine }, "stubs": stubs });
    if let Some(port) = port {
        doc["port"] = json!(port);
    }
    serde_json::from_value(doc).expect("config")
}

struct Twin {
    manager: ImposterManager,
    port: u16,
    client: reqwest::Client,
}

impl Twin {
    async fn new(stubs: Vec<Value>) -> Self {
        Self::with_engine(stubs, json!({})).await
    }

    async fn with_engine(stubs: Vec<Value>, engine: Value) -> Self {
        let manager = ImposterManager::new();
        let port = manager
            .create_imposter(config(None, stubs, engine))
            .await
            .expect("create");
        Self {
            manager,
            port,
            client: reqwest::Client::new(),
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .send()
            .await
            .expect("request")
    }

    /// The count the twin answered, or the status it was not a count.
    async fn count(&self, path: &str) -> Result<u64, u16> {
        let resp = self.get(path).await;
        match resp.headers().get("x-rift-latency-ms") {
            Some(v) => Ok(v.to_str().expect("text").parse().expect("number")),
            None => Err(resp.status().as_u16()),
        }
    }

    fn instances(&self) -> usize {
        self.manager
            .get_imposter(self.port)
            .expect("imposter")
            .wasm_instances()
            .list()
            .len()
    }

    async fn stop(self) {
        self.manager
            .delete_imposter(self.port)
            .await
            .expect("delete");
    }
}

// The issue's table: each edit, then one call. Edits that keep the twin's component, export and
// knobs referenced keep its count; the others start it again.
#[tokio::test]
async fn the_twin_survives_edits_that_keep_it_referenced() {
    let knobs = json!({ "region": "eu" });
    let t = Twin::new(vec![twin("a", knobs.clone()), sibling("one")]).await;
    for n in 1..=3 {
        assert_eq!(t.count("/c").await, Ok(n));
    }
    assert_eq!(t.instances(), 1);
    let port = t.port;
    let m = &t.manager;

    // A fault window: a 503 in front of the twin, then removed.
    m.add_stub(port, stub(outage()), Some(0))
        .await
        .expect("add");
    assert_eq!(t.count("/c").await, Err(503), "the twin is not called");
    m.delete_stub(port, 0).await.expect("delete");
    assert_eq!(t.count("/c").await, Ok(4));

    // Every stub replaced: the same twin, a changed sibling.
    m.replace_stubs(
        port,
        vec![stub(twin("a", knobs.clone())), stub(sibling("two"))],
    )
    .await
    .expect("replace all");
    assert_eq!(t.count("/c").await, Ok(5));

    // Moved.
    m.move_stub(port, 0, 1).await.expect("move");
    assert_eq!(t.count("/c").await, Ok(6));

    // Replaced in place with a predicate added: same component and knobs.
    let mut predicated = twin("a", knobs.clone());
    predicated["predicates"] = json!([{ "equals": { "method": "GET" } }]);
    m.replace_stub(port, 1, stub(predicated.clone()))
        .await
        .expect("replace");
    assert_eq!(t.count("/c").await, Ok(7));

    // A `_rift.fault` on the twin stub that never fires.
    let mut faulted = predicated.clone();
    faulted["responses"][0]["_rift"]["fault"] =
        json!({ "error": { "probability": 0.0, "status": 500 } });
    m.replace_stub(port, 1, stub(faulted.clone()))
        .await
        .expect("fault");
    assert_eq!(t.count("/c").await, Ok(8));

    // A whole-config apply that patches the sibling only.
    m.apply_config(vec![config(
        Some(port),
        vec![sibling("three"), faulted.clone()],
        json!({}),
    )])
    .await
    .expect("apply");
    assert_eq!(t.count("/c").await, Ok(9));
    assert_eq!(t.instances(), 1);

    // Different knobs: a new key, the old one unreferenced.
    m.replace_stub(port, 1, stub(twin("a", json!({ "region": "us" }))))
        .await
        .expect("knobs");
    assert_eq!(t.count("/c").await, Ok(1));
    assert_eq!(t.instances(), 1, "the old instance is dropped");

    // A different component.
    m.replace_stub(port, 1, stub(twin("b", json!({ "region": "us" }))))
        .await
        .expect("sha");
    assert_eq!(t.count("/c").await, Ok(1));

    // Deleted and added again: nothing referenced it in between.
    assert_eq!(t.count("/c").await, Ok(2));
    m.delete_stub(port, 1).await.expect("delete twin");
    assert_eq!(t.instances(), 0);
    m.add_stub(port, stub(twin("b", json!({ "region": "us" }))), None)
        .await
        .expect("re-add");
    assert_eq!(t.count("/c").await, Ok(1));

    // A runtime restart.
    assert_eq!(t.count("/c").await, Ok(2));
    m.restart_runtime_state(port).await.expect("restart");
    assert_eq!(t.instances(), 0);
    assert_eq!(t.count("/c").await, Ok(1));
    t.stop().await;
}

#[tokio::test]
async fn request_mode_starts_every_call_afresh() {
    let mut request = twin("a", json!({}));
    request["responses"][0]["_rift"]["script"]["instance"] = json!("request");
    let t = Twin::new(vec![request]).await;
    for _ in 0..3 {
        assert_eq!(t.count("/c").await, Ok(1));
    }
    assert_eq!(t.instances(), 0);
    t.stop().await;
}

// A trap drops the instance: the next call starts from scratch.
#[tokio::test]
async fn a_trap_drops_the_instance() {
    let t = Twin::new(vec![twin("a", json!({}))]).await;
    assert_eq!(t.count("/c").await, Ok(1));
    assert_eq!(t.count("/c").await, Ok(2));
    assert_eq!(t.count("/trap").await, Err(500));
    assert_eq!(t.instances(), 0);
    assert_eq!(t.count("/c").await, Ok(1));
    t.stop().await;
}

// One key, one instance: two stubs with the same component and knobs share a count; different
// knobs keep two.
#[tokio::test]
async fn a_key_is_shared_and_knobs_separate_instances() {
    let routed = |path: &str, knobs: Value| {
        let mut stub = twin("a", knobs);
        stub["predicates"] = json!([{ "startsWith": { "path": path } }]);
        stub
    };
    let t = Twin::new(vec![
        routed("/x", json!({ "k": 1 })),
        routed("/y", json!({ "k": 1 })),
        routed("/z", json!({ "k": 2 })),
    ])
    .await;
    assert_eq!(t.count("/x1").await, Ok(1));
    assert_eq!(t.count("/y1").await, Ok(2), "the same instance");
    assert_eq!(t.count("/z1").await, Ok(1), "its own instance");
    assert_eq!(t.instances(), 2);
    t.stop().await;
}

// Calls on one instance run one at a time: concurrent calls each see their own count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_on_one_instance_serialize() {
    let t = Twin::new(vec![twin("a", json!({}))]).await;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let client = t.client.clone();
        let url = format!("http://127.0.0.1:{}/c", t.port);
        set.spawn(async move {
            let resp = client.get(url).send().await.expect("request");
            resp.headers()["x-rift-latency-ms"]
                .to_str()
                .expect("text")
                .parse::<u64>()
                .expect("number")
        });
    }
    let mut counts = Vec::new();
    while let Some(count) = set.join_next().await {
        counts.push(count.expect("join"));
    }
    counts.sort_unstable();
    assert_eq!(counts, (1..=20).collect::<Vec<_>>());
    t.stop().await;
}

// A call outlives its client: a client that hangs up mid-call does not cut the guest, and the
// instance keeps what the call did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_outlives_a_client_that_hangs_up() {
    let t = Twin::new(vec![twin("a", json!({}))]).await;
    assert_eq!(t.count("/c").await, Ok(1));
    let hung_up = t
        .client
        .get(format!("http://127.0.0.1:{}/sleep", t.port))
        .timeout(Duration::from_millis(50))
        .send()
        .await;
    assert!(hung_up.is_err(), "the client gave up before the answer");
    // The guest finishes its 300 ms; the next call waits for the instance and then counts on.
    assert_eq!(t.count("/c").await, Ok(3));
    t.stop().await;
}

// The values a call reads that are the request's, not the instance's: its random stream (with
// rngKey, each key its own) and its clock.
#[tokio::test]
async fn each_call_reads_its_own_request_stream_and_clock() {
    let keyed = json!({ "seed": 7, "rngKey": ["header:x-key"] });
    let random = |instance: &str| {
        json!({ "responses": [{ "_rift": { "script": {
            "engine": "wasm",
            "code": wasm_fixtures::b64(&wasm_fixtures::random_body()),
            "instance": instance,
        } } }] })
    };
    // A kept instance reads each request's stream: with rngKey, a key draws the same value
    // whatever order the requests arrive in, as it does with a fresh instance per call.
    let draws = |order: [&'static str; 2]| {
        let keyed = keyed.clone();
        async move {
            let t = Twin::with_engine(vec![random("imposter")], keyed).await;
            let mut out = std::collections::BTreeMap::new();
            for key in order {
                let resp = t
                    .client
                    .get(format!("http://127.0.0.1:{}/r", t.port))
                    .header("x-key", key)
                    .send()
                    .await
                    .expect("request");
                out.insert(key, resp.text().await.expect("body"));
            }
            assert_eq!(t.instances(), 1, "one instance served both keys");
            t.stop().await;
            out
        }
    };
    let ab = draws(["a", "b"]).await;
    assert_eq!(ab, draws(["b", "a"]).await);
    assert_ne!(ab["a"], ab["b"]);

    let clocked = json!({ "responses": [{ "_rift": { "script": {
        "engine": "wasm",
        "code": wasm_fixtures::b64(&wasm_fixtures::wall_clock_seconds()),
        "instance": "imposter",
    } } }] });
    let t = Twin::with_engine(vec![clocked], json!({ "clock": "header" })).await;
    // The guest answers `delay(<wall seconds>)`: small instants keep the delay short.
    let at = |ms: i64| {
        let client = t.client.clone();
        let url = format!("http://127.0.0.1:{}/t", t.port);
        async move {
            let resp = client
                .get(url)
                .header("x-rift-now", ms.to_string())
                .send()
                .await
                .expect("request");
            resp.headers()["x-rift-latency-ms"]
                .to_str()
                .expect("text")
                .to_owned()
        }
    };
    assert_eq!(at(1_000).await, "1");
    assert_eq!(at(5_000).await, "5");
    assert_eq!(t.instances(), 1);
    t.stop().await;
}

// A kept instance does not keep its imposter: deleting the imposter frees it (the instance's store
// held the last request's flow store, which holds the imposter).
#[tokio::test]
async fn a_deleted_imposter_is_freed_with_its_instances() {
    let t = Twin::new(vec![twin("a", json!({}))]).await;
    assert_eq!(t.count("/c").await, Ok(1));
    let weak = std::sync::Arc::downgrade(&t.manager.get_imposter(t.port).expect("imposter"));
    t.manager.delete_imposter(t.port).await.expect("delete");
    for _ in 0..100 {
        if weak.upgrade().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the deleted imposter is still alive");
}

// A restart that cuts a call mid-way leaves the new generation a fresh instance: the old call
// finishes on the old one, which is not put back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_mid_call_starts_the_next_generation_afresh() {
    let t = Twin::new(vec![twin("a", json!({}))]).await;
    assert_eq!(t.count("/c").await, Ok(1));
    let url = format!("http://127.0.0.1:{}/sleep", t.port);
    let client = t.client.clone();
    let sleeping = tokio::spawn(async move { client.get(url).send().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    t.manager
        .restart_runtime_state(t.port)
        .await
        .expect("restart");
    let _ = sleeping.await.expect("join");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        t.count("/c").await,
        Ok(1),
        "the new generation's own instance"
    );
    assert_eq!(t.instances(), 1);
    t.stop().await;
}

// A call that misses its deadline while the guest runs drops the instance.
#[tokio::test]
async fn a_deadline_miss_drops_the_instance() {
    // Compiled now, so the tight budget below covers calls only.
    let bytes = wasm_fixtures::memory_counter("deadline");
    rift_mock_core::scripting::admit_wasm_component(&wasm_fixtures::b64(&bytes), None)
        .expect("admitted");
    let t = Twin::with_engine(
        vec![twin("deadline", json!({}))],
        json!({ "timeoutMs": 250 }),
    )
    .await;
    assert_eq!(t.count("/c").await, Ok(1));
    assert_eq!(t.count("/c").await, Ok(2));
    assert_eq!(t.count("/sleep").await, Err(504));
    assert_eq!(t.instances(), 0, "the instance is dropped");
    // A fresh instance counts from 1. The budget is tight on purpose (the guest's sleep must miss
    // it), so a loaded host may miss it once more while instantiating.
    let mut next = t.count("/c").await;
    for _ in 0..3 {
        if next != Err(504) {
            break;
        }
        next = t.count("/c").await;
    }
    assert_eq!(next, Ok(1));
    t.stop().await;
}

// A `component: "sha256:…"` reference and inline `code` of the same bytes are one component, so
// one instance.
#[tokio::test]
async fn a_component_reference_and_its_code_share_an_instance() {
    let bytes = wasm_fixtures::memory_counter("shared");
    let manager = ImposterManager::new();
    let (entry, _) = manager.components().insert(&bytes).expect("upload");
    let by_code = json!({ "predicates": [{ "equals": { "path": "/cd" } }],
        "responses": [{ "_rift": { "script": {
            "engine": "wasm", "code": wasm_fixtures::b64(&bytes), "instance": "imposter" } } }] });
    let by_reference = json!({ "predicates": [{ "equals": { "path": "/rf" } }],
        "responses": [{ "_rift": { "script": {
            "component": format!("sha256:{}", entry.id.hex()), "instance": "imposter" } } }] });
    let port = manager
        .create_imposter(config(None, vec![by_code, by_reference], json!({})))
        .await
        .expect("create");
    let t = Twin {
        manager,
        port,
        client: reqwest::Client::new(),
    };
    assert_eq!(t.count("/cd").await, Ok(1));
    assert_eq!(t.count("/rf").await, Ok(2));
    assert_eq!(t.count("/cd").await, Ok(3));
    assert_eq!(t.instances(), 1);
    t.stop().await;
}
