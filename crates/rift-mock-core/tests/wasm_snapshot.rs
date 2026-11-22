//! Issue #1794: a kept wasm instance's state, read through its optional `rift:mock/introspect`
//! export, without perturbing the run.
//!
//! The component is `introspect_counter`: each `respond` adds one to a wasm global and answers
//! `delay(<count>)`, and `snapshot` returns `{"count":"NN"}` (or the failure a test asks for).
#![cfg(feature = "wasm")]

#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use rift_mock_core::scripting::{WasmSnapshot, WasmSnapshotError};
use serde_json::{Value, json};
use std::sync::Arc;

fn stub(bytes: &[u8], instance: Option<&str>, knobs: Value, path: Option<&str>) -> Value {
    let mut script = json!({ "engine": "wasm", "code": wasm_fixtures::b64(bytes), "knobs": knobs });
    if let Some(instance) = instance {
        script["instance"] = json!(instance);
    }
    let mut stub = json!({ "responses": [{ "_rift": { "script": script } }] });
    if let Some(path) = path {
        stub["predicates"] = json!([{ "startsWith": { "path": path } }]);
    }
    stub
}

fn kept(snapshot: &str) -> Value {
    stub(
        &wasm_fixtures::introspect_counter(snapshot),
        Some("imposter"),
        json!({}),
        None,
    )
}

struct Twin {
    manager: ImposterManager,
    port: u16,
    client: reqwest::Client,
}

impl Twin {
    async fn new(stubs: Vec<Value>, engine: Value) -> Self {
        let manager = ImposterManager::new();
        let config: ImposterConfig = serde_json::from_value(
            json!({ "protocol": "http", "_rift": { "scriptEngine": engine }, "stubs": stubs }),
        )
        .expect("config");
        let port = manager.create_imposter(config).await.expect("create");
        Self {
            manager,
            port,
            client: reqwest::Client::new(),
        }
    }

    /// The delay the twin answered: its count, or its draw.
    async fn count(&self, path: &str) -> Result<u64, u16> {
        let resp = self
            .client
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .send()
            .await
            .expect("request");
        match resp.headers().get("x-rift-latency-ms") {
            Some(v) => Ok(v.to_str().expect("text").parse().expect("number")),
            None => Err(resp.status().as_u16()),
        }
    }

    async fn snapshot(&self, id: Option<&str>) -> Result<WasmSnapshot, WasmSnapshotError> {
        self.manager
            .get_imposter(self.port)
            .expect("imposter")
            .wasm_snapshot(id)
            .await
    }

    fn instances(&self) -> Vec<rift_mock_core::scripting::WasmInstanceInfo> {
        self.manager
            .get_imposter(self.port)
            .expect("imposter")
            .wasm_instances()
            .list()
    }

    async fn stop(self) {
        self.manager
            .delete_imposter(self.port)
            .await
            .expect("delete");
    }
}

// Three requests, then a snapshot: the guest's text byte for byte, the fuel it spent, and no call
// counted. The next request counts on from 3, so the snapshot ran neither `init` nor `respond`.
#[tokio::test]
async fn a_snapshot_reads_the_guest_state_and_is_not_a_call() {
    let t = Twin::new(vec![kept("count")], json!({})).await;
    for n in 1..=3 {
        assert_eq!(t.count("/c").await, Ok(n));
    }
    let snapshot = t.snapshot(None).await.expect("snapshot");
    assert_eq!(snapshot.body, r#"{"count":"03"}"#);
    assert!(snapshot.fuel_used.is_some_and(|f| f > 0), "{snapshot:?}");
    assert_eq!(t.instances()[0].calls, 3, "a snapshot is not a call");
    // Again: reading twice reads the same state.
    assert_eq!(
        t.snapshot(None).await.expect("again").body,
        r#"{"count":"03"}"#
    );
    assert_eq!(t.count("/c").await, Ok(4));
    t.stop().await;
}

// The guest's failures, each named, and which of them keep the instance.
#[tokio::test]
async fn each_failure_is_named_and_only_a_trap_drops_the_instance() {
    let no_export = stub(
        &wasm_fixtures::memory_counter("no-introspect"),
        Some("imposter"),
        json!({}),
        None,
    );
    // The twin, what its snapshot fails with, and whether the instance is kept.
    type Case = (Value, fn(&WasmSnapshotError) -> bool, bool);
    let cases: [Case; 4] = [
        (
            no_export,
            |e| matches!(e, WasmSnapshotError::NoExport),
            true,
        ),
        (
            kept("err"),
            |e| matches!(e, WasmSnapshotError::Guest(m) if m == "nope"),
            true,
        ),
        (
            kept("not-json"),
            |e| matches!(e, WasmSnapshotError::NotJson),
            true,
        ),
        (
            kept("trap"),
            |e| matches!(e, WasmSnapshotError::Trap(_)),
            false,
        ),
    ];
    for (twin, expected, keeps) in cases {
        let t = Twin::new(vec![twin], json!({})).await;
        assert_eq!(t.count("/c").await, Ok(1));
        let got = t.snapshot(None).await;
        assert!(got.as_ref().is_err_and(expected), "{got:?}");
        assert_eq!(t.instances().len(), usize::from(keeps), "{got:?}");
        assert_eq!(
            t.count("/c").await,
            Ok(if keeps { 2 } else { 1 }),
            "{got:?}: a kept instance counts on, a dropped one starts again"
        );
        t.stop().await;
    }
}

// `state` is read-only during a snapshot: the write returns the refusal, and nothing is stored.
#[tokio::test]
async fn a_snapshot_cannot_write_state() {
    let t = Twin::new(vec![kept("write")], json!({})).await;
    assert_eq!(t.count("/c").await, Ok(1));
    let got = t.snapshot(None).await;
    assert!(
        matches!(&got, Err(WasmSnapshotError::Guest(m)) if m.contains("read-only during snapshot")),
        "{got:?}"
    );
    let imposter = t.manager.get_imposter(t.port).expect("imposter");
    assert_eq!(
        imposter.flow_store.get("", "k").expect("get"),
        None,
        "the refused write stored nothing"
    );
    assert_eq!(t.count("/c").await, Ok(2), "the instance is kept");
    t.stop().await;
}

// The route never instantiates: before the first call there is nothing to read, and an imposter
// whose wasm stubs are all `instance: "request"` keeps no instance at all.
#[tokio::test]
async fn nothing_is_instantiated_to_answer_a_snapshot() {
    let t = Twin::new(vec![kept("count")], json!({})).await;
    assert_eq!(
        t.snapshot(None).await,
        Err(WasmSnapshotError::NotInstantiated)
    );
    assert!(t.instances().is_empty(), "the snapshot made none");
    assert_eq!(t.count("/c").await, Ok(1), "and ran no init or respond");
    t.stop().await;

    let request_mode = stub(
        &wasm_fixtures::introspect_counter("count"),
        None,
        json!({}),
        None,
    );
    let t = Twin::new(vec![request_mode], json!({})).await;
    assert_eq!(t.count("/c").await, Ok(1));
    assert_eq!(t.snapshot(None).await, Err(WasmSnapshotError::RequestMode));
    t.stop().await;

    // No wasm stub at all is not request mode: there is just no instance.
    let plain = json!({ "responses": [{ "is": { "statusCode": 200 } }] });
    let t = Twin::new(vec![plain], json!({})).await;
    assert_eq!(
        t.snapshot(None).await,
        Err(WasmSnapshotError::NotInstantiated)
    );
    t.stop().await;
}

// Two live instances: a snapshot must name one. Each id reads its own instance's state; an id no
// instance has is not instantiated.
#[tokio::test]
async fn two_instances_need_an_id() {
    let bytes = wasm_fixtures::introspect_counter("count");
    let t = Twin::new(
        vec![
            stub(&bytes, Some("imposter"), json!({ "n": 1 }), Some("/one")),
            stub(&bytes, Some("imposter"), json!({ "n": 2 }), Some("/two")),
        ],
        json!({}),
    )
    .await;
    assert_eq!(t.count("/one").await, Ok(1));
    for n in 1..=2 {
        assert_eq!(t.count("/two").await, Ok(n));
    }
    let instances = t.instances();
    assert_eq!(instances.len(), 2);
    match t.snapshot(None).await {
        Err(WasmSnapshotError::Ambiguous(ids)) => {
            let mut listed: Vec<_> = instances.iter().map(|i| i.id.clone()).collect();
            listed.sort();
            assert_eq!(ids, listed);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    let mut bodies: Vec<String> = Vec::new();
    for info in &instances {
        bodies.push(t.snapshot(Some(&info.id)).await.expect("named").body);
    }
    bodies.sort();
    assert_eq!(bodies, [r#"{"count":"01"}"#, r#"{"count":"02"}"#]);
    assert_eq!(
        t.snapshot(Some("0000")).await,
        Err(WasmSnapshotError::NotInstantiated)
    );
    t.stop().await;
}

// A seeded imposter replays byte for byte with snapshots in between, even though `snapshot` draws
// from `wasi:random`: it draws from a throwaway stream, not the imposter's.
#[tokio::test]
async fn snapshots_do_not_perturb_a_seeded_run() {
    async fn run(with_snapshots: bool) -> Vec<u64> {
        let t = Twin::new(vec![kept("random")], json!({ "seed": 42 })).await;
        let mut draws = Vec::new();
        for _ in 0..5 {
            draws.push(t.count("/c").await.expect("a draw"));
            if with_snapshots {
                t.snapshot(None).await.expect("snapshot");
                t.snapshot(None).await.expect("snapshot");
            }
        }
        t.stop().await;
        draws
    }
    let plain = run(false).await;
    assert_eq!(plain, run(true).await);
    assert_eq!(plain, run(false).await, "the run is seeded");
}

// Snapshots taken while requests run see whole calls only: each is a count some prefix of the
// calls reached, never one a call was half-way through, and they never go backwards.
#[tokio::test]
async fn a_snapshot_under_load_sees_a_consistent_prefix() {
    let t = Arc::new(Twin::new(vec![kept("count")], json!({})).await);
    assert_eq!(t.count("/c").await, Ok(1));
    let mut requests = Vec::new();
    for _ in 0..16 {
        let t = Arc::clone(&t);
        requests.push(tokio::spawn(async move { t.count("/c").await }));
    }
    let mut seen = Vec::new();
    for _ in 0..16 {
        let body = t.snapshot(None).await.expect("snapshot").body;
        let value: Value = serde_json::from_str(&body).expect("JSON");
        let n: u64 = value["count"]
            .as_str()
            .expect("count")
            .parse()
            .expect("digits");
        seen.push(n);
        tokio::task::yield_now().await;
    }
    for request in requests {
        assert!(request.await.expect("task").is_ok());
    }
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    assert!(seen.iter().all(|&n| (1..=17).contains(&n)), "{seen:?}");
    let last = t.snapshot(None).await.expect("snapshot").body;
    assert_eq!(last, r#"{"count":"17"}"#);
    Arc::into_inner(t).expect("one owner").stop().await;
}
