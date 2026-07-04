//! Bounded execution for the inline `_rift.script` (should_inject) path (issue #308).
//!
//! The imposter `_rift.script` path has no script pool, so a runaway script (e.g. an
//! infinite Rhai `loop {}`) ran unbounded on the async worker and wedged the whole engine.
//! This module runs the script off the async worker via `spawn_blocking` and interrupts it
//! at a wall-clock deadline using the same abort-flag mechanism as the pooled path (#172):
//! Rhai's `on_progress` callback, and a Lua instruction hook.

use super::{FaultDecision, ScriptEngine, ScriptRequest};
use crate::extensions::flow_state::FlowStore;
use crate::imposter::ImposterConfig;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
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

/// Run a `_rift.script` `should_inject` off the async worker with a wall-clock deadline
/// (issue #308). Execution happens in `spawn_blocking` so a non-yielding script cannot
/// starve the Tokio runtime, and at `timeout` the abort flag is set so the script self-
/// interrupts. Rhai (`on_progress`) and Lua (instruction hook) are truly interrupted and
/// free their thread promptly. JavaScript can't observe the deadline flag mid-run (Boa has no
/// per-instruction interrupt), but its context caps loop iterations (issue #327), so a runaway
/// loop terminates by throwing instead of leaking its blocking thread forever. Returns `Err` on
/// timeout, a compile/exec error, or a panic.
pub async fn should_inject_bounded(
    engine_type: String,
    code: String,
    rule_id: String,
    request: ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
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
        )
    });

    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(anyhow::anyhow!("script task panicked: {join_err}")),
        Err(_elapsed) => {
            // Signal the deadline: Rhai/Lua self-interrupt and free their thread promptly;
            // for other engines this is a best-effort flag they never observe.
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

/// Execute `should_inject` synchronously with the abort flag wired into the interpreter, so
/// setting `abort` interrupts a runaway script. Rhai and Lua get a real interpreter interrupt
/// (#308/#172); other engines run without an interpreter interrupt but still off the async
/// worker and under the request-level timeout.
fn run_should_inject_with_abort(
    engine_type: &str,
    code: &str,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    abort: &Arc<AtomicBool>,
) -> Result<FaultDecision> {
    // Start each execution with a clean last-flow-error slot so `flow_store.last_error()` can't
    // observe a stale error left by a previous script on this reused worker thread (issue #322).
    crate::extensions::flow_state::clear_last_flow_error();
    match engine_type {
        "rhai" => super::rhai_engine::run_should_inject_with_abort_rhai(
            code, rule_id, request, flow_store, abort,
        ),
        #[cfg(feature = "lua")]
        "lua" => super::lua_engine::run_should_inject_with_abort_lua(
            code, rule_id, request, flow_store, abort,
        ),
        other => {
            let engine = ScriptEngine::new(other, code, rule_id)?;
            engine.should_inject_fault(request, flow_store)
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

    const RUNAWAY_RHAI: &str =
        "fn should_inject(request, flow_store){ let i = 0; loop { i += 1; } }";
    const RUNAWAY_LUA: &str = "function should_inject(request, flow_store) while true do end end";

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
            let res = run_should_inject_with_abort(engine, code, "t", &req(), store(), &abort);
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

    // AC5: same for Lua.
    #[cfg(feature = "lua")]
    #[test]
    fn runaway_lua_interrupted_by_abort() {
        assert_interrupts("lua", RUNAWAY_LUA);
    }

    // The non-rhai/non-lua dispatch branch: an unknown engine returns an error promptly.
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
        let none = bounded(
            "rhai",
            "fn should_inject(request, flow_store){ #{ inject: false } }",
            2000,
        )
        .await
        .expect("fast script");
        assert!(matches!(none, FaultDecision::None), "got {none:?}");

        let latency = bounded(
            "rhai",
            "fn should_inject(request, flow_store){ #{ inject: true, fault: `latency`, duration_ms: 7 } }",
            2000,
        )
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

    // AC4: a normal (fast) Rhai should_inject still returns the correct decision, unchanged.
    #[tokio::test]
    async fn normal_rhai_returns_error_decision() {
        let code = "fn should_inject(request, flow_store){ #{ inject: true, fault: `error`, status: 503, body: `boom` } }";
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

    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn normal_lua_returns_decision() {
        let code = "function should_inject(request, flow_store) return { inject = true, fault = 'error', status = 503, body = 'boom' } end";
        let res = bounded("lua", code, 2000)
            .await
            .expect("fast lua script succeeds");
        match res {
            FaultDecision::Error { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Error decision, got {other:?}"),
        }
    }
}
