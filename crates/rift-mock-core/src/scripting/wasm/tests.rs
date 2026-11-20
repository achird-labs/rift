//! The wasm engine's unit gate (issue #1751): decisions, limits, the import policy, admission and
//! the cache, against components built from core-module text (`test-support/wasm_fixtures.rs`).

#[path = "../../../test-support/wasm_fixtures.rs"]
mod fixtures;

use super::runtime::{CallReport, TrapKind, compile_count};
use super::{WasmAdmitError, admit, respond};
use crate::backends::InMemoryFlowStore;
use crate::extensions::flow_state::FlowStore;
use crate::imposter::{ResponseMode, WasmLimits};
use crate::scripting::{
    FaultDecision, ScriptCtxExtras, ScriptRequest, ScriptTimeoutError, WasmScript,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn req() -> ScriptRequest {
    ScriptRequest {
        mode: ResponseMode::Text,
        raw_body: Some(String::new()),
        method: "GET".into(),
        path: "/x".into(),
        headers: Default::default(),
        body: serde_json::Value::Null,
        query: Default::default(),
        path_params: Default::default(),
    }
}

fn store() -> Arc<dyn FlowStore> {
    Arc::new(InMemoryFlowStore::new(3600))
}

async fn call(
    bytes: &[u8],
    limits: WasmLimits,
    knobs: Option<&serde_json::Value>,
    timeout_ms: u64,
    flow_store: Arc<dyn FlowStore>,
    request: ScriptRequest,
) -> (anyhow::Result<FaultDecision>, CallReport) {
    let code = fixtures::b64(bytes);
    respond(
        WasmScript {
            code: &code,
            export: None,
            knobs,
            limits,
            clock: None,
        },
        "t",
        &request,
        flow_store,
        Duration::from_millis(timeout_ms),
        &ScriptCtxExtras {
            flow_id: Some("flow-1".into()),
            ..Default::default()
        },
    )
    .await
}

async fn run_with(
    bytes: &[u8],
    limits: WasmLimits,
    timeout_ms: u64,
) -> (anyhow::Result<FaultDecision>, CallReport) {
    call(bytes, limits, None, timeout_ms, store(), req()).await
}

async fn run(bytes: &[u8]) -> anyhow::Result<FaultDecision> {
    run_with(bytes, WasmLimits::default(), 5000).await.0
}

fn body(decision: FaultDecision) -> String {
    match decision {
        FaultDecision::Error { body, .. } => body,
        other => panic!("expected respond, got {other:?}"),
    }
}

#[tokio::test]
async fn respond_maps_to_an_http_decision_with_headers() {
    match run(&fixtures::respond_ok()).await.expect("decision") {
        FaultDecision::Error {
            status,
            body,
            headers,
            rule_id,
        } => {
            assert_eq!(status, 200);
            assert_eq!(body, "ok");
            assert_eq!(headers.get("x-twin").map(String::as_str), Some("yes"));
            assert_eq!(rule_id, "t");
        }
        other => panic!("expected respond, got {other:?}"),
    }
}

#[tokio::test]
async fn pass_delay_and_reset_map_like_rhai() {
    assert!(matches!(
        run(&fixtures::respond_bare(0)).await.expect("pass"),
        FaultDecision::None
    ));
    assert!(matches!(
        run(&fixtures::respond_bare(2)).await.expect("reset"),
        FaultDecision::Reset { .. }
    ));
    match run(&fixtures::respond_delay_250()).await.expect("delay") {
        FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 250),
        other => panic!("expected delay, got {other:?}"),
    }
}

#[tokio::test]
async fn a_guest_error_surfaces_as_its_string() {
    let (result, report) = run_with(
        &fixtures::respond_guest_error(),
        WasmLimits::default(),
        5000,
    )
    .await;
    assert_eq!(
        result.expect_err("guest error").to_string(),
        "guest says no"
    );
    assert_eq!(
        report.trap_text().as_deref(),
        Some("guest-error: guest says no")
    );
}

#[tokio::test]
async fn an_init_error_is_a_guest_error() {
    let (result, report) = run_with(&fixtures::init_error(), WasmLimits::default(), 5000).await;
    assert_eq!(
        result.expect_err("init error").to_string(),
        "init: bad knobs"
    );
    assert_eq!(
        report.trap_text().as_deref(),
        Some("guest-error: init: bad knobs")
    );
}

#[tokio::test]
async fn knobs_reach_init_as_json_text_and_default_to_an_empty_object() {
    let knobs = serde_json::json!({"limit": 2});
    let (with, _) = call(
        &fixtures::respond_knobs(),
        WasmLimits::default(),
        Some(&knobs),
        5000,
        store(),
        req(),
    )
    .await;
    assert_eq!(body(with.expect("knobs")), r#"{"limit":2}"#);
    assert_eq!(
        body(run(&fixtures::respond_knobs()).await.expect("none")),
        "{}"
    );
}

// A text body reaches the guest as its bytes; a binary-mode body (base64 on Rift's side) reaches it
// decoded.
#[tokio::test]
async fn the_request_body_reaches_the_guest_as_bytes() {
    let text = ScriptRequest {
        raw_body: Some("hello".into()),
        ..req()
    };
    let (echoed, _) = call(
        &fixtures::respond_echo_body(),
        WasmLimits::default(),
        None,
        5000,
        store(),
        text,
    )
    .await;
    assert_eq!(body(echoed.expect("text")), "hello");
    let binary = ScriptRequest {
        raw_body: Some("aGk=".into()),
        mode: ResponseMode::Binary,
        ..req()
    };
    let (echoed, _) = call(
        &fixtures::respond_echo_body(),
        WasmLimits::default(),
        None,
        5000,
        store(),
        binary,
    )
    .await;
    assert_eq!(body(echoed.expect("binary")), "hi");
}

#[tokio::test]
async fn a_body_that_is_not_utf8_is_a_script_error() {
    let err = run(&fixtures::respond_binary_body())
        .await
        .expect_err("binary body");
    assert_eq!(
        err.to_string(),
        "response body is not UTF-8; use export \"http\" (#1758) for binary bodies"
    );
}

// The deadline interrupts a guest that never returns, and the guest yields while it spins: a
// sibling task on the same single-threaded runtime keeps running.
#[tokio::test(flavor = "current_thread")]
async fn a_spinning_guest_is_interrupted_at_its_deadline_and_yields_meanwhile() {
    let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = Arc::clone(&ticks);
    let sibling = tokio::spawn(async move {
        for _ in 0..1000 {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let start = Instant::now();
    let (result, report) = run_with(&fixtures::spin(), WasmLimits::default(), 200).await;
    let elapsed = start.elapsed();
    sibling.abort();
    let err = result.expect_err("spin must not return");
    let timeout = err
        .downcast_ref::<ScriptTimeoutError>()
        .unwrap_or_else(|| panic!("expected a ScriptTimeoutError, got {err}"));
    assert_eq!(timeout.hook, "_rift.script");
    assert_eq!(timeout.timeout_ms, 200);
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    assert!(
        ticks.load(std::sync::atomic::Ordering::Relaxed) >= 3,
        "the sibling task must run while the guest spins"
    );
    assert_eq!(report.trap_text().as_deref(), Some("deadline"));
}

// A `timeoutMs` past what `Instant` can hold is no epoch deadline, not a panic.
#[tokio::test]
async fn a_huge_timeout_does_not_overflow_the_deadline() {
    let (result, _) = run_with(&fixtures::respond_bare(0), WasmLimits::default(), u64::MAX).await;
    assert!(matches!(result.expect("pass"), FaultDecision::None));
}

#[tokio::test]
async fn a_fuel_budget_stops_a_busy_guest_and_is_counted() {
    let before = crate::extensions::metrics::WASM_TRAPS_TOTAL
        .with_label_values(&["fuel"])
        .get();
    let limits = WasmLimits {
        fuel: Some(1000),
        ..WasmLimits::default()
    };
    let (result, report) = run_with(&fixtures::spin(), limits, 5000).await;
    assert_eq!(
        result.expect_err("fuel").to_string(),
        "fuel exhausted (1000 units)"
    );
    assert_eq!(report.fuel_used, Some(1000));
    assert_eq!(report.trap_text().as_deref(), Some("fuel"));
    let after = crate::extensions::metrics::WASM_TRAPS_TOTAL
        .with_label_values(&["fuel"])
        .get();
    assert!(after >= before + 1.0, "{before} -> {after}");
}

#[tokio::test]
async fn fuel_used_is_reported_without_a_budget() {
    let (result, report) = run_with(&fixtures::respond_bare(0), WasmLimits::default(), 5000).await;
    result.expect("pass");
    assert!(
        report.fuel_used.is_some_and(|f| f > 0),
        "{:?}",
        report.fuel_used
    );
}

#[tokio::test]
async fn growing_memory_past_the_cap_fails_naming_the_limit() {
    let (result, report) = run_with(&fixtures::grow_1gib(), WasmLimits::default(), 5000).await;
    let msg = result.expect_err("memory cap").to_string();
    assert!(
        msg.starts_with("memory limit exceeded (maxMemoryMiB 64): "),
        "{msg}"
    );
    assert!(
        matches!(report.trap, Some((TrapKind::Memory, _))),
        "{:?}",
        report.trap
    );
}

// The cap is the call's, not one memory's: two memories, each under it, are refused together.
#[tokio::test]
async fn the_memory_cap_counts_every_memory_of_the_call() {
    let limits = WasmLimits {
        max_memory_mib: 40,
        ..WasmLimits::default()
    };
    // 2 × 24 MiB (384 pages each) is under 40 MiB per memory and over it together.
    let (result, report) = run_with(&fixtures::two_memories(384), limits, 5000).await;
    let msg = result.expect_err("two memories").to_string();
    assert!(
        msg.starts_with("memory limit exceeded (maxMemoryMiB 40): "),
        "{msg}"
    );
    assert!(matches!(report.trap, Some((TrapKind::Memory, _))));
    // Under the cap together, the same component runs.
    let (result, _) = run_with(&fixtures::two_memories(64), limits, 5000).await;
    assert!(matches!(result.expect("2 × 4 MiB"), FaultDecision::None));
}

#[tokio::test]
async fn a_higher_memory_cap_admits_the_same_growth() {
    let limits = WasmLimits {
        max_memory_mib: 1100,
        ..WasmLimits::default()
    };
    let (result, _) = run_with(&fixtures::grow_1gib(), limits, 5000).await;
    assert!(matches!(result.expect("grows"), FaultDecision::None));
}

// `unreachable` is a trap of kind `trap`; another component still answers after it.
#[tokio::test]
async fn an_unreachable_trap_fails_the_call() {
    let (result, report) =
        run_with(&fixtures::unreachable_trap(), WasmLimits::default(), 5000).await;
    let msg = result.expect_err("trap").to_string();
    assert!(msg.starts_with("wasm trap: "), "{msg}");
    assert!(
        matches!(report.trap, Some((TrapKind::Trap, _))),
        "{:?}",
        report.trap
    );
    assert!(matches!(
        run(&fixtures::respond_bare(0))
            .await
            .expect("other component"),
        FaultDecision::None
    ));
}

#[tokio::test]
async fn state_incr_by_writes_the_requests_flow_in_the_shared_store() {
    let shared = store();
    for _ in 0..2 {
        call(
            &fixtures::incr_count(),
            WasmLimits::default(),
            None,
            5000,
            Arc::clone(&shared),
            req(),
        )
        .await
        .0
        .expect("incr");
    }
    assert_eq!(
        shared.get("flow-1", "count").expect("get"),
        Some(serde_json::json!(2))
    );
}

// Every call gets its own store: concurrent calls share only the flow store, and lose no update.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_each_get_a_store_and_share_the_flow_store() {
    let shared = store();
    let calls: Vec<_> = (0..32)
        .map(|_| {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                call(
                    &fixtures::incr_count(),
                    WasmLimits::default(),
                    None,
                    5000,
                    shared,
                    req(),
                )
                .await
                .0
                .is_ok()
            })
        })
        .collect();
    for call in calls {
        assert!(call.await.expect("join"));
    }
    assert_eq!(
        shared.get("flow-1", "count").expect("get"),
        Some(serde_json::json!(32))
    );
}

/// A store that says it blocks, and records the thread each call ran on.
struct BlockingStore {
    inner: InMemoryFlowStore,
    threads: parking_lot::Mutex<Vec<std::thread::ThreadId>>,
}

impl FlowStore for BlockingStore {
    fn get(&self, f: &str, k: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.inner.get(f, k)
    }
    fn set(&self, f: &str, k: &str, v: serde_json::Value) -> anyhow::Result<()> {
        self.inner.set(f, k, v)
    }
    fn exists(&self, f: &str, k: &str) -> anyhow::Result<bool> {
        self.inner.exists(f, k)
    }
    fn delete(&self, f: &str, k: &str) -> anyhow::Result<()> {
        self.inner.delete(f, k)
    }
    fn is_blocking(&self) -> bool {
        true
    }
    fn increment(&self, f: &str, k: &str) -> anyhow::Result<i64> {
        self.inner.increment(f, k)
    }
    fn increment_by(&self, f: &str, k: &str, by: i64) -> anyhow::Result<i64> {
        self.threads.lock().push(std::thread::current().id());
        self.inner.increment_by(f, k, by)
    }
    fn set_ttl(&self, f: &str, ttl: i64) -> anyhow::Result<()> {
        self.inner.set_ttl(f, ttl)
    }
}

// A blocking store is called off the request task (issue #475), as `ctx.state` calls it.
#[tokio::test(flavor = "current_thread")]
async fn a_blocking_store_is_called_off_the_request_task() {
    let blocking = Arc::new(BlockingStore {
        inner: InMemoryFlowStore::new(3600),
        threads: parking_lot::Mutex::new(Vec::new()),
    });
    let shared: Arc<dyn FlowStore> = blocking.clone();
    call(
        &fixtures::incr_count(),
        WasmLimits::default(),
        None,
        5000,
        shared,
        req(),
    )
    .await
    .0
    .expect("incr");
    let threads = blocking.threads.lock().clone();
    assert_eq!(threads.len(), 1, "one incr-by call");
    assert_ne!(threads[0], std::thread::current().id());
    assert_eq!(
        blocking.inner.get("flow-1", "count").expect("get"),
        Some(serde_json::json!(1))
    );
}

#[test]
fn admission_refuses_sockets_and_http_imports_by_name() {
    assert_eq!(
        admit(&fixtures::b64(&fixtures::importing("sockets")), None),
        Err(WasmAdmitError::Import("wasi:sockets/tcp@0.2.6".into()))
    );
    assert_eq!(
        admit(&fixtures::b64(&fixtures::importing("http")), None),
        Err(WasmAdmitError::Import(
            "wasi:http/outgoing-handler@0.2.6".into()
        ))
    );
    assert_eq!(
        WasmAdmitError::Import("wasi:sockets/tcp@0.2.6".into()).to_string(),
        "wasm component imports wasi:sockets/tcp@0.2.6, which Rift does not provide (allowed: \
         wasi:cli, wasi:io, wasi:filesystem, wasi:clocks, wasi:random, rift:mock/state)"
    );
}

#[test]
fn admission_accepts_a_standard_wasi_cli_import() {
    assert_eq!(
        admit(&fixtures::b64(&fixtures::importing("cli")), None),
        Ok(())
    );
}

// The environment Rift links is empty: a guest asking for it gets no variables, not the host's
// (this test process has some, PATH among them).
#[tokio::test]
async fn a_guest_reading_the_environment_gets_nothing() {
    assert!(
        std::env::vars().next().is_some(),
        "the host has an environment"
    );
    match run(&fixtures::respond_env_count()).await.expect("env read") {
        FaultDecision::Error { status, .. } => assert_eq!(status, 200, "200 + 0 variables"),
        other => panic!("expected respond, got {other:?}"),
    }
}

#[test]
fn admission_refuses_a_core_module_a_world_mismatch_and_inline_source() {
    match admit(&fixtures::b64(&fixtures::core_module()), None) {
        Err(WasmAdmitError::NotComponent(_)) => {}
        other => panic!("expected NotComponent, got {other:?}"),
    }
    assert!(
        WasmAdmitError::NotComponent("x".into())
            .to_string()
            .starts_with("not a component; build for wasm32-wasip2")
    );
    // Base64 that is not wasm is a component error, not an inline source.
    match admit(&fixtures::b64(b"hello"), None) {
        Err(WasmAdmitError::NotComponent(_)) => {}
        other => panic!("expected NotComponent, got {other:?}"),
    }
    match admit(&fixtures::b64(&fixtures::no_handler()), None) {
        Err(WasmAdmitError::World(msg)) => {
            assert!(msg.contains("rift:mock/handler"), "{msg}");
        }
        other => panic!("expected World, got {other:?}"),
    }
    match admit("fn respond(ctx) { pass() }", None) {
        Err(e @ WasmAdmitError::NotBase64(_)) => assert!(
            e.to_string()
                .starts_with("engine \"wasm\" has no inline form; use file ("),
            "{e}"
        ),
        other => panic!("expected NotBase64, got {other:?}"),
    }
}

#[test]
fn admission_refuses_an_export_other_than_rift() {
    let code = fixtures::b64(&fixtures::respond_ok());
    assert_eq!(admit(&code, Some("rift")), Ok(()));
    assert_eq!(
        admit(&code, Some("http")),
        Err(WasmAdmitError::Export("http".into()))
    );
}

/// A component no other test uses: a data segment carrying `tag`.
fn unique(tag: &str) -> Vec<u8> {
    fixtures::component(
        "mock",
        &fixtures::core(
            "",
            &format!(
                r#"(data (i32.const 64) "\00") (data (i32.const 72) "\00") (data (i32.const 2000) "{tag}")"#
            ),
            "(i32.const 64)",
        ),
    )
}

// Same bytes compile once, wherever they are admitted or served from; new bytes compile again.
#[tokio::test]
async fn the_cache_compiles_each_component_once() {
    let a = fixtures::b64(&unique("cache-test-a"));
    let b = fixtures::b64(&unique("cache-test-b"));
    assert_eq!(compile_count(&a), 0);
    admit(&a, None).expect("admit");
    // A second imposter carrying the same bytes.
    admit(&a, None).expect("admit again");
    run(&unique("cache-test-a")).await.expect("serve");
    run(&unique("cache-test-a")).await.expect("serve again");
    assert_eq!(compile_count(&a), 1);
    admit(&b, None).expect("admit other");
    assert_eq!(compile_count(&b), 1);
    assert_eq!(compile_count(&a), 1);
}

// A miss on serve (admitted before a restart) compiles the component instead of failing.
#[tokio::test]
async fn a_component_not_yet_cached_is_compiled_when_served() {
    let fresh = unique("cold-serve");
    let code = fixtures::b64(&fresh);
    assert_eq!(compile_count(&code), 0);
    assert!(matches!(
        run(&fresh).await.expect("served"),
        FaultDecision::None
    ));
    assert_eq!(compile_count(&code), 1);
}

async fn delay_with_clock(bytes: &[u8], clock: Option<&crate::scripting::ScriptClock>) -> u64 {
    let code = fixtures::b64(bytes);
    let (result, _) = respond(
        WasmScript {
            code: &code,
            export: None,
            knobs: None,
            limits: WasmLimits::default(),
            clock,
        },
        "t",
        &req(),
        store(),
        Duration::from_millis(5000),
        &ScriptCtxExtras::default(),
    )
    .await;
    match result.expect("clock read") {
        FaultDecision::Latency { duration_ms, .. } => duration_ms,
        other => panic!("expected delay, got {other:?}"),
    }
}

// Issue #1754: a component's `wasi:clocks` read the imposter's frozen instant, a request's
// `X-Rift-Now`, or the system clock.
#[tokio::test]
async fn wasi_clocks_read_the_frozen_or_fixed_instant() {
    use crate::scripting::ScriptClock;
    let clock = std::sync::Arc::new(crate::imposter::clock::ImposterClock::default());
    clock.freeze(1_700_000_000_000).expect("freeze");
    let imposter = ScriptClock::Imposter(std::sync::Arc::clone(&clock));
    assert_eq!(
        delay_with_clock(&fixtures::wall_clock_seconds(), Some(&imposter)).await,
        1_700_000_000
    );
    assert_eq!(
        delay_with_clock(&fixtures::monotonic_nanos(), Some(&imposter)).await,
        1_700_000_000_000_000_000
    );
    // A step taken after the store exists is seen at the next read: read at every call.
    clock.advance(5_000).expect("advance");
    assert_eq!(
        delay_with_clock(&fixtures::wall_clock_seconds(), Some(&imposter)).await,
        1_700_000_005
    );
    let fixed = ScriptClock::Fixed(1_000_000);
    assert_eq!(
        delay_with_clock(&fixtures::wall_clock_seconds(), Some(&fixed)).await,
        1_000
    );
    // Unfrozen: wall time, read between two host readings (a second can turn over in between).
    clock.reset();
    let secs = || u64::try_from(crate::imposter::clock::wall_ms() / 1000).expect("positive");
    for script_clock in [Some(&imposter), None] {
        let before = secs();
        let read = delay_with_clock(&fixtures::wall_clock_seconds(), script_clock).await;
        let after = secs();
        assert!(
            (before..=after).contains(&read),
            "{read} outside {before}..={after}"
        );
    }
}

// Monotonic reads never go back between calls, though each call has a fresh store.
#[tokio::test]
async fn the_monotonic_clock_does_not_go_back_between_calls() {
    use crate::scripting::ScriptClock;
    let wall = ScriptClock::Imposter(std::sync::Arc::default());
    let first = delay_with_clock(&fixtures::monotonic_nanos(), Some(&wall)).await;
    let second = delay_with_clock(&fixtures::monotonic_nanos(), Some(&wall)).await;
    assert!(second >= first, "{first} then {second}");
    let frozen = ScriptClock::Fixed(42);
    assert_eq!(
        delay_with_clock(&fixtures::monotonic_nanos(), Some(&frozen)).await,
        delay_with_clock(&fixtures::monotonic_nanos(), Some(&frozen)).await
    );
}
