//! What a `_rift.script` costs per engine (issue #1760).
//!
//! - `cold_compile/wasm`: compiling and linking a component, bypassing the cache: what admitting
//!   one costs. Set `RIFT_BENCH_COMPONENT` to a built `.wasm` (e.g. `examples/wasm/counter-twin`,
//!   built for `wasm32-wasip2`) to measure a real toolchain's output; otherwise a minimal
//!   hand-written component is compiled, which only shows the fixed cost.
//! - `warm_instantiate/wasm`: a fresh `Store`, instantiating the cached component and `init`:
//!   the part of every wasm call that comes before `respond`.
//! - `warm_call/{rhai,js,wasm}`: a trivial handler answering 200 through the same public entry
//!   points the imposter handler uses, engine caches warm. The wasm figure includes the fresh
//!   `Store` and instantiation, as every request pays them.
//! - `state_roundtrip/{rhai,js,wasm}`: a handler that increments a counter in the in-memory flow
//!   store and answers.
//!
//! `cargo bench -p rift-mock-core --features wasm --bench script_engine_bench`; under `cargo test`
//! each benchmark runs once as a smoke test.

#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use criterion::{Criterion, criterion_group, criterion_main};
use rift_mock_core::backends::InMemoryFlowStore;
use rift_mock_core::extensions::flow_state::FlowStore;
use rift_mock_core::imposter::{ResponseMode, WasmLimits};
use rift_mock_core::scripting::{
    FaultDecision, ScriptCtxExtras, ScriptRequest, WasmScript, should_inject_bounded_with_ctx,
    should_inject_wasm, wasm_bench,
};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;

const TIMEOUT: Duration = Duration::from_secs(5);

const RHAI_OK: &str = r#"fn respond(ctx) { http(200, "ok") }"#;
const JS_OK: &str = r#"function respond(ctx) { return http(200, "ok"); }"#;
const RHAI_STATE: &str = r#"fn respond(ctx) { let n = ctx.state.incr("count"); http(200, "ok") }"#;
const JS_STATE: &str =
    r#"function respond(ctx) { var n = ctx.state.incr("count"); return http(200, "ok"); }"#;

fn request() -> ScriptRequest {
    ScriptRequest {
        method: "GET".into(),
        path: "/bench".into(),
        headers: Default::default(),
        body: serde_json::Value::Null,
        query: Default::default(),
        path_params: Default::default(),
        raw_body: None,
        mode: ResponseMode::Text,
    }
}

fn extras() -> ScriptCtxExtras {
    ScriptCtxExtras {
        flow_id: Some("bench".into()),
        ..Default::default()
    }
}

fn store() -> Arc<dyn FlowStore> {
    Arc::new(InMemoryFlowStore::new(300))
}

/// A real component when `RIFT_BENCH_COMPONENT` names one, else the minimal fixture.
fn compile_subject() -> String {
    match std::env::var_os("RIFT_BENCH_COMPONENT") {
        Some(path) => wasm_fixtures::b64(
            &std::fs::read(&path).unwrap_or_else(|e| panic!("RIFT_BENCH_COMPONENT {path:?}: {e}")),
        ),
        None => wasm_fixtures::b64(&wasm_fixtures::respond_ok()),
    }
}

async fn script(engine: &str, code: &str, store: &Arc<dyn FlowStore>) -> FaultDecision {
    should_inject_bounded_with_ctx(
        engine.to_owned(),
        code.to_owned(),
        "bench".to_owned(),
        request(),
        Arc::clone(store),
        TIMEOUT,
        extras(),
    )
    .await
    .expect("script runs")
}

async fn wasm(code: &str, store: &Arc<dyn FlowStore>) -> FaultDecision {
    should_inject_wasm(
        WasmScript {
            code,
            export: None,
            knobs: None,
            limits: WasmLimits::default(),
            clock: None,
        },
        "bench",
        &request(),
        Arc::clone(store),
        TIMEOUT,
        &extras(),
    )
    .await
    .expect("component runs")
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn cold_compile(c: &mut Criterion) {
    let code = compile_subject();
    let mut group = c.benchmark_group("cold_compile");
    group.sample_size(10);
    group.bench_function("wasm", |b| {
        b.iter(|| wasm_bench::compile_uncached(black_box(&code)).expect("compiles"));
    });
    group.finish();
}

fn warm_instantiate(c: &mut Criterion) {
    let rt = runtime();
    let code = wasm_fixtures::b64(&wasm_fixtures::respond_ok());
    let store = store();
    rt.block_on(wasm_bench::instantiate(&code, Arc::clone(&store)))
        .expect("instantiates");
    let mut group = c.benchmark_group("warm_instantiate");
    group.bench_function("wasm", |b| {
        b.to_async(&rt)
            .iter(|| wasm_bench::instantiate(black_box(&code), Arc::clone(&store)));
    });
    group.finish();
}

/// One group, the same handler in each engine: `rhai`/`js` source, and the wasm component.
fn engines(c: &mut Criterion, name: &str, rhai: &str, js: &str, component: &[u8]) {
    let rt = runtime();
    let store = store();
    let wasm_code = wasm_fixtures::b64(component);
    // Warm every engine's cache, and check the handler answers rather than errors.
    rt.block_on(async {
        script("rhai", rhai, &store).await;
        script("javascript", js, &store).await;
        wasm(&wasm_code, &store).await;
    });
    let mut group = c.benchmark_group(name);
    group.bench_function("rhai", |b| {
        b.to_async(&rt).iter(|| script("rhai", rhai, &store));
    });
    group.bench_function("js", |b| {
        b.to_async(&rt).iter(|| script("javascript", js, &store));
    });
    group.bench_function("wasm", |b| {
        b.to_async(&rt).iter(|| wasm(&wasm_code, &store));
    });
    group.finish();
}

fn warm_call(c: &mut Criterion) {
    engines(c, "warm_call", RHAI_OK, JS_OK, &wasm_fixtures::respond_ok());
}

fn state_roundtrip(c: &mut Criterion) {
    engines(
        c,
        "state_roundtrip",
        RHAI_STATE,
        JS_STATE,
        &wasm_fixtures::incr_count(),
    );
}

criterion_group!(
    benches,
    cold_compile,
    warm_instantiate,
    warm_call,
    state_roundtrip
);
criterion_main!(benches);
