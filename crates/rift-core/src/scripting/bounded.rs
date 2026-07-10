//! Bounded execution for the inline `_rift.script` (`respond(ctx)`) path (issue #308).
//!
//! The imposter `_rift.script` path has no script pool, so a runaway script (e.g. an
//! infinite Rhai `loop {}`) ran unbounded on the async worker and wedged the whole engine.
//! This module runs the script off the async worker via `spawn_blocking` and interrupts it
//! at a wall-clock deadline using the same abort-flag mechanism as the pooled path (#172):
//! Rhai's `on_progress` callback.

use super::{
    FaultDecision, ScriptCtxExtras, ScriptEngine, ScriptRequest, ScriptTraceEntry,
    capture_script_logs, render_decision,
};
use crate::extensions::flow_state::FlowStore;
use crate::imposter::ImposterConfig;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

/// Default script timeout when `_rift.scriptEngine.timeoutMs` is not configured.
pub const DEFAULT_SCRIPT_TIMEOUT_MS: u64 = 5000;

/// The `_rift.script` wall-clock deadline for an imposter: `_rift.scriptEngine.timeoutMs`
/// if configured, else [`DEFAULT_SCRIPT_TIMEOUT_MS`] (issue #308).
pub fn resolve_script_timeout_ms(config: &ImposterConfig) -> u64 {
    config
        .rift
        .as_ref()
        .and_then(|r| r.script_engine.as_ref())
        .map(|se| se.timeout_ms)
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS)
}

/// Run a `_rift.script` `respond(ctx)` off the async worker with a wall-clock deadline
/// (issue #308). Execution happens in `spawn_blocking` so a non-yielding script cannot
/// starve the Tokio runtime, and at `timeout` the abort flag is set so the script self-
/// interrupts. Rhai (`on_progress`) is truly interrupted and frees its thread promptly.
/// JavaScript can't observe the deadline flag mid-run (Boa has no per-instruction interrupt),
/// but its context caps loop iterations (issue #327), so a runaway loop terminates by throwing
/// instead of leaking its blocking thread forever. Returns `Err` on timeout, a compile/exec
/// error, or a panic.
pub async fn should_inject_bounded(
    engine_type: String,
    code: String,
    rule_id: String,
    request: ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
) -> Result<FaultDecision> {
    should_inject_bounded_with_ctx(
        engine_type,
        code,
        rule_id,
        request,
        flow_store,
        timeout,
        ScriptCtxExtras::default(),
    )
    .await
}

/// As [`should_inject_bounded`], but threading real `ctx.flowId`/`ctx.stub` context (issue #357
/// Item 1) through to the v2 `ctx` object — used by the imposter `_rift.script` hook, which knows
/// the resolved flow id and matched stub.
pub async fn should_inject_bounded_with_ctx(
    engine_type: String,
    code: String,
    rule_id: String,
    request: ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
    ctx_extra: ScriptCtxExtras,
) -> Result<FaultDecision> {
    let abort = Arc::new(AtomicBool::new(false));
    let run_abort = Arc::clone(&abort);
    let handle = tokio::task::spawn_blocking(move || {
        run_should_inject_with_abort(
            &engine_type,
            &code,
            &rule_id,
            &request,
            flow_store,
            &run_abort,
            &ctx_extra,
        )
    });

    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(anyhow::anyhow!("script task panicked: {join_err}")),
        Err(_elapsed) => {
            // Signal the deadline: Rhai self-interrupts and frees its thread promptly; for
            // other engines this is a best-effort flag they never observe.
            abort.store(true, Ordering::Relaxed);
            warn!(
                "_rift.script execution timed out after {}ms",
                timeout.as_millis()
            );
            Err(anyhow::anyhow!(
                "script execution timed out after {}ms",
                timeout.as_millis()
            ))
        }
    }
}

/// As [`should_inject_bounded_with_ctx`], but also builds a debug-mode script trace (issue #360
/// Item 3): the rendered decision, wall-clock duration, and any `ctx.logger` lines this run
/// emitted. Only called when debug mode is on — [`should_inject_bounded_with_ctx`] above is
/// unchanged and stays the zero-cost default the rest of the time (no capturing subscriber, no
/// extra `Instant`/allocation).
pub async fn should_inject_bounded_with_ctx_traced(
    engine_type: String,
    code: String,
    rule_id: String,
    request: ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
    ctx_extra: ScriptCtxExtras,
) -> (Result<FaultDecision>, ScriptTraceEntry) {
    let abort = Arc::new(AtomicBool::new(false));
    let run_abort = Arc::clone(&abort);
    let start = Instant::now();
    let handle = tokio::task::spawn_blocking(move || {
        capture_script_logs(|| {
            run_should_inject_with_abort(
                &engine_type,
                &code,
                &rule_id,
                &request,
                flow_store,
                &run_abort,
                &ctx_extra,
            )
        })
    });

    let (result, logs) = match tokio::time::timeout(timeout, handle).await {
        Ok(Ok((result, logs))) => (result, logs),
        Ok(Err(join_err)) => (
            Err(anyhow::anyhow!("script task panicked: {join_err}")),
            Vec::new(),
        ),
        Err(_elapsed) => {
            abort.store(true, Ordering::Relaxed);
            warn!(
                "_rift.script execution timed out after {}ms",
                timeout.as_millis()
            );
            (
                Err(anyhow::anyhow!(
                    "script execution timed out after {}ms",
                    timeout.as_millis()
                )),
                Vec::new(),
            )
        }
    };
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let decision = match &result {
        Ok(d) => render_decision(d),
        Err(e) => format!("error: {e}"),
    };
    // Raw (uncapped) logs here: `rift script run` prints them all to the terminal. The
    // header-bound debug trace caps them at its own serialization site (issue #360) — a
    // response header, unlike a terminal dump, must stay bounded.
    let entry = ScriptTraceEntry {
        hook: "respond".to_string(),
        decision,
        duration_ms,
        logs,
        cache: None,
    };
    (result, entry)
}

/// Execute `respond(ctx)` synchronously with the abort flag wired into the interpreter, so
/// setting `abort` interrupts a runaway script. Rhai gets a real interpreter interrupt
/// (#308/#172); other engines run without an interpreter interrupt but still off the async
/// worker and under the request-level timeout.
fn run_should_inject_with_abort(
    engine_type: &str,
    code: &str,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    abort: &Arc<AtomicBool>,
    ctx_extra: &ScriptCtxExtras,
) -> Result<FaultDecision> {
    // Start each execution with a clean last-flow-error slot so `flow_store.last_error()` can't
    // observe a stale error left by a previous script on this reused worker thread (issue #322).
    crate::extensions::flow_state::clear_last_flow_error();
    match engine_type {
        "rhai" => super::rhai_engine::run_should_inject_with_abort_rhai(
            code, rule_id, request, flow_store, abort, ctx_extra,
        ),
        other => {
            let engine = ScriptEngine::new(other, code, rule_id)?;
            engine.should_inject_fault_with_ctx(request, flow_store, ctx_extra)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::flow_state::NoOpFlowStore;
    use std::time::Instant;

    fn req() -> ScriptRequest {
        ScriptRequest {
            raw_body: None,
            method: "GET".into(),
            path: "/hang".into(),
            headers: Default::default(),
            body: serde_json::Value::Null,
            query: Default::default(),
            path_params: Default::default(),
        }
    }

    fn store() -> Arc<dyn FlowStore> {
        Arc::new(NoOpFlowStore)
    }

    /// Thin wrapper over [`should_inject_bounded`] with the fixed test request/store, to
    /// keep the call sites to just engine/code/timeout.
    fn bounded(
        engine: &'static str,
        code: &'static str,
        timeout_ms: u64,
    ) -> impl std::future::Future<Output = Result<FaultDecision>> {
        should_inject_bounded(
            engine.into(),
            code.into(),
            "t".into(),
            req(),
            store(),
            Duration::from_millis(timeout_ms),
        )
    }

    // A bare-expression script (issue #357 Item 2, no `respond` wrapper needed): the whole body
    // runs as the entrypoint, so this loop actually executes (unlike a function that's merely
    // declared but never called), exercising the real interrupt path.
    const RUNAWAY_RHAI: &str = "let i = 0; loop { i += 1; }";

    /// Run the sync interrupt path on a child thread, flip the abort flag after 200ms, and
    /// require the interpreter to unwind within 5s. Running off the test thread with a
    /// `recv_timeout` makes a regression that fails to interrupt FAIL the test (the channel
    /// times out) rather than hang the whole suite.
    fn assert_interrupts(engine: &'static str, code: &'static str) {
        let abort = Arc::new(AtomicBool::new(false));
        let a2 = Arc::clone(&abort);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            a2.store(true, Ordering::Relaxed);
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = run_should_inject_with_abort(
                engine,
                code,
                "t",
                &req(),
                store(),
                &abort,
                &ScriptCtxExtras::default(),
            );
            let _ = tx.send(res.is_err());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(is_err) => assert!(is_err, "an interrupted script returns an error"),
            Err(_) => panic!("runaway {engine} was NOT interrupted by the abort flag within 5s"),
        }
    }

    // AC1/AC5: a runaway Rhai script is TRULY interrupted (the interpreter unwinds) once the
    // abort flag is set — not merely abandoned.
    #[test]
    fn runaway_rhai_interrupted_by_abort() {
        assert_interrupts("rhai", RUNAWAY_RHAI);
    }

    // The non-rhai dispatch branch: an unknown engine returns an error promptly.
    #[tokio::test]
    async fn unknown_engine_returns_error() {
        let res = bounded("no-such-engine", "whatever", 500).await;
        assert!(
            res.is_err(),
            "an unknown engine is a fast error, not a hang"
        );
    }

    // AC4: the None and Latency decisions pass through the bounded path unchanged.
    #[tokio::test]
    async fn normal_rhai_returns_none_and_latency() {
        let none = bounded("rhai", "fn respond(ctx) { pass() }", 2000)
            .await
            .expect("fast script");
        assert!(matches!(none, FaultDecision::None), "got {none:?}");

        let latency = bounded("rhai", "fn respond(ctx) { delay(7) }", 2000)
            .await
            .expect("fast script");
        match latency {
            FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 7),
            other => panic!("expected Latency, got {other:?}"),
        }
    }

    // AC2: the handler resolves the deadline from `_rift.scriptEngine.timeoutMs`, else the
    // default — proving the config plumbing, not just an explicit Duration.
    #[test]
    fn resolves_configured_and_default_timeout() {
        let with_timeout: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "_rift": { "scriptEngine": { "timeoutMs": 321 } },
            "stubs": []
        }))
        .expect("config");
        assert_eq!(resolve_script_timeout_ms(&with_timeout), 321);

        // scriptEngine present but timeoutMs omitted → serde default (kept in sync).
        let engine_no_timeout: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "_rift": { "scriptEngine": { "defaultEngine": "rhai" } },
            "stubs": []
        }))
        .expect("config");
        assert_eq!(
            resolve_script_timeout_ms(&engine_no_timeout),
            DEFAULT_SCRIPT_TIMEOUT_MS
        );

        // no scriptEngine block at all → the handler fallback default.
        let no_engine: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http", "stubs": []
        }))
        .expect("config");
        assert_eq!(
            resolve_script_timeout_ms(&no_engine),
            DEFAULT_SCRIPT_TIMEOUT_MS
        );
    }

    // AC1/AC2: the async entrypoint times out a runaway script and returns an error,
    // instead of hanging.
    #[tokio::test]
    async fn should_inject_bounded_times_out_runaway_rhai() {
        let start = Instant::now();
        let res = bounded("rhai", RUNAWAY_RHAI, 200).await;
        assert!(res.is_err(), "runaway must yield an error, not hang");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "must return near the configured timeout, not hang"
        );
    }

    // AC2: the deadline honors the configured timeout, not a hardcoded larger default.
    #[tokio::test]
    async fn bounded_honors_configured_timeout() {
        let start = Instant::now();
        let _ = bounded("rhai", RUNAWAY_RHAI, 250).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200),
            "must not return before the configured timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must return at ~the configured timeout, not a 5s default: {elapsed:?}"
        );
    }

    // AC3: a runaway script must not starve the async runtime — a concurrent tokio timer
    // makes progress and completes well before the script's (much longer) timeout.
    #[tokio::test]
    async fn runaway_does_not_starve_async_runtime() {
        let script = tokio::spawn(bounded("rhai", RUNAWAY_RHAI, 4000));

        // This timer must fire promptly while the script is still running.
        let timer_start = Instant::now();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let timer_elapsed = timer_start.elapsed();
        assert!(
            timer_elapsed < Duration::from_secs(1),
            "async runtime is starved by the runaway script: timer took {timer_elapsed:?}"
        );

        // Let the script's own deadline clean it up.
        let _ = script.await;
    }

    // AC4: a normal (fast) Rhai respond(ctx) still returns the correct decision, unchanged.
    #[tokio::test]
    async fn normal_rhai_returns_error_decision() {
        let code = r#"fn respond(ctx) { http(503, "boom") }"#;
        let res = bounded("rhai", code, 2000)
            .await
            .expect("fast script succeeds");
        match res {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 503);
                assert_eq!(body, "boom");
            }
            other => panic!("expected Error decision, got {other:?}"),
        }
    }

    // Issue #360 Item 3: the traced variant captures the decision, duration, and ctx.logger
    // lines from a single run, without changing the decision itself.
    #[tokio::test]
    async fn traced_variant_captures_decision_duration_and_logs() {
        let code = r#"
            fn respond(ctx) {
                ctx.logger.info("about to respond");
                http(503, "boom")
            }
        "#;
        let (result, entry) = should_inject_bounded_with_ctx_traced(
            "rhai".into(),
            code.into(),
            "t".into(),
            req(),
            store(),
            Duration::from_millis(2000),
            ScriptCtxExtras::default(),
        )
        .await;
        match result {
            Ok(FaultDecision::Error { status, .. }) => assert_eq!(status, 503),
            other => panic!("expected Error decision, got {other:?}"),
        }
        assert_eq!(entry.hook, "respond");
        assert_eq!(entry.decision, "http(503) body=\"boom\"");
        assert_eq!(entry.logs, vec!["about to respond".to_string()]);
        assert!(entry.cache.is_none());
    }

    // A timeout still produces a trace entry (best-effort: no logs, an error decision string),
    // instead of panicking or losing the timeout error.
    #[tokio::test]
    async fn traced_variant_reports_timeout() {
        let (result, entry) = should_inject_bounded_with_ctx_traced(
            "rhai".into(),
            RUNAWAY_RHAI.into(),
            "t".into(),
            req(),
            store(),
            Duration::from_millis(150),
            ScriptCtxExtras::default(),
        )
        .await;
        assert!(result.is_err(), "runaway must time out, not hang");
        assert!(
            entry.decision.starts_with("error:"),
            "got {}",
            entry.decision
        );
    }

    // =====================================================================================
    // Issue #476: Mountebank response-inject hook runs off the async worker with a
    // wall-clock deadline, through the same spawn_blocking + timeout shape as `_rift.script`.
    // =====================================================================================
    #[cfg(feature = "javascript")]
    mod mb_inject {
        use super::*;
        use crate::scripting::{MountebankRequest, execute_mountebank_inject_bounded};
        use std::collections::HashMap;
        use std::sync::atomic::AtomicU32;

        // Fresh port per test so parallel tests never share `IMPOSTER_STATE`; range disjoint
        // from js_engine.rs and imposter/response.rs test ranges.
        fn test_port() -> u16 {
            static NEXT: AtomicU32 = AtomicU32::new(42_500);
            NEXT.fetch_add(1, Ordering::Relaxed) as u16
        }

        fn mb_req(path: &str) -> MountebankRequest {
            MountebankRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            }
        }

        // Busy-loops long enough that a small wall-clock deadline always fires first; the
        // loop-iteration cap (#327) later frees the blocking thread.
        const SLOW_INJECT: &str = "function (config) { var i = 0; while (i < 100000000) { i += 1; } return { statusCode: 200 }; }";

        // AC1 (happy): a fast inject returns its response through the bounded path.
        #[tokio::test]
        async fn mb_inject_bounded_returns_response() {
            let resp = execute_mountebank_inject_bounded(
                "function (config) { return { statusCode: 201, body: 'from-inject' }; }".into(),
                mb_req("/a"),
                test_port(),
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("fast inject");
            assert_eq!(resp.status_code, 201);
            assert_eq!(resp.body, "from-inject");
        }

        // AC1: a runaway inject yields a timeout error near the deadline instead of blocking
        // a runtime worker for its full duration.
        #[tokio::test]
        async fn mb_inject_bounded_times_out() {
            let start = Instant::now();
            let res = execute_mountebank_inject_bounded(
                SLOW_INJECT.into(),
                mb_req("/hang"),
                test_port(),
                None,
                Duration::from_millis(25),
            )
            .await;
            let err = match res {
                Err(e) => e.to_string(),
                Ok(_) => panic!("runaway inject must yield an error, not a response"),
            };
            assert!(err.contains("timed out"), "got: {err}");
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "must return near the configured deadline, not after the loop cap"
            );
        }

        // AC4: per-imposter script state persists across bounded runs — the get→run→save
        // plumbing (issue #477 locking included) follows execution to the blocking thread.
        #[tokio::test]
        async fn mb_inject_bounded_state_persists_across_calls() {
            let port = test_port();
            let incr = "function (config) { config.state.count = (config.state.count || 0) + 1; return { statusCode: 200, body: String(config.state.count) }; }";
            let first = execute_mountebank_inject_bounded(
                incr.into(),
                mb_req("/count"),
                port,
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("first run");
            let second = execute_mountebank_inject_bounded(
                incr.into(),
                mb_req("/count"),
                port,
                None,
                Duration::from_millis(60_000),
            )
            .await
            .expect("second run");
            assert_eq!(first.body, "1");
            assert_eq!(
                second.body, "2",
                "imposter state must persist across pool-thread runs"
            );
        }
    }
}
