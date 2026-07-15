use crate::extensions::flow_state::{CasOutcome, FlowStore, flow_result};
use crate::imposter::ResponseMode;
use anyhow::{Result, anyhow};
use rhai::Dynamic;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// Engine modules (only used by proxy.rs for compilation)
mod bounded;
mod compiled_cache;
mod rhai_engine;
pub use bounded::{
    DEFAULT_SCRIPT_TIMEOUT_MS, ScriptTimeoutError, resolve_script_timeout_ms,
    should_inject_bounded, should_inject_bounded_with_ctx, should_inject_bounded_with_ctx_traced,
};

pub use rhai_engine::RhaiEngine;
/// Exposed for the decision-cache payoff bench (issue #665): the worker-side execute with a
/// reusable engine, i.e. the per-request cost a cache hit actually avoids. `should_inject_fault`
/// is not a substitute there — it builds a fresh `Engine` per call, which the pool never pays.
pub use rhai_engine::execute_rhai_with_engine;

// Script pool for optimized execution
mod script_pool;
pub use script_pool::{CompiledScript, ScriptPool, ScriptPoolConfig};

// Decision cache for memoization
mod decision_cache;
pub use decision_cache::{CacheKey, CacheKeyBody, DecisionCache, DecisionCacheConfig};

#[cfg(feature = "javascript")]
mod js_engine;
/// Exposed so other modules (e.g. `behaviors::wait`) that run a standalone JS snippet outside the
/// MB inject/predicate/decorate hooks still get the same interpreter-level guards (issue #355
/// Items 3/6) rather than an unbounded `Context::default()`.
#[cfg(feature = "javascript")]
pub(crate) use js_engine::bounded_js_context;
#[cfg(feature = "javascript")]
pub use js_engine::{
    JsEngine, MountebankRequest, PredicateInjectionError, clear_imposter_state,
    compile_js_to_bytecode, execute_mountebank_config_decorate, execute_mountebank_decorate,
    execute_mountebank_inject, execute_mountebank_inject_bounded,
    execute_predicate_generator_inject, execute_predicate_inject,
};
#[cfg(feature = "javascript")]
#[allow(unused_imports)]
pub use js_engine::{MountebankDecorateResponse, MountebankInjectResponse, execute_js_bytecode};

// Validator trait and unified error types
mod validator;
#[allow(unused_imports)]
pub use validator::ScriptValidationError;
pub use validator::ScriptValidator;

// Validator modules - used by config validation and stub_validator
mod rhai_validator;
#[allow(unused_imports)]
pub use rhai_validator::RhaiValidationError;
pub use rhai_validator::RhaiValidator;

#[cfg(feature = "javascript")]
mod js_validator;
#[cfg(feature = "javascript")]
#[allow(unused_imports)]
pub use js_validator::JsValidationError;
#[cfg(feature = "javascript")]
pub use js_validator::JsValidator;

// Stub script validation for Admin API
mod stub_validator;
pub use stub_validator::{validate_stub, validate_stubs};

// Static entrypoint/arity checking, for `rift script check` (issue #360)
mod entrypoint_check;
pub use entrypoint_check::{EntrypointCheckError, EntrypointMatch, check_entrypoint};

// Script decision/log tracing, for `rift script run` and the debug-mode per-request trace
// (issue #360)
mod trace;
pub use trace::{ScriptTraceEntry, cap_trace_logs, capture_script_logs, render_decision};

/// Failure of a `predicateGenerators.inject` pass during proxy recording (issue #498).
///
/// Distinguishes "predicates could NOT be generated" (infrastructure, script, or malformed
/// output) from a generator legitimately returning an empty list. Without this distinction the
/// proxy-recording path collapsed every failure to an empty predicate list and silently recorded
/// a match-all stub; carrying the failure as an error lets the caller skip auto-stub creation.
/// Defined unconditionally so the proxy signature resolves without the `javascript` feature.
#[derive(Debug, thiserror::Error)]
pub enum PredicateGeneratorError {
    /// The MB script pool could not run the generator (infrastructure failure).
    #[error("script pool failure: {0}")]
    Pool(String),
    /// The generator script errored while executing.
    #[error("script execution error: {0}")]
    Script(String),
    /// The generator ran but did not return a usable predicate array.
    #[error("generator produced invalid output: {0}")]
    Output(String),
}

impl PredicateGeneratorError {
    /// Short, stable, header-safe token for the `x-rift-generator-error` response header
    /// (the full detail goes to the server log, not to the client).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Pool(_) => "pool-failure",
            Self::Script(_) => "script-error",
            Self::Output(_) => "invalid-output",
        }
    }
}

/// Script execution result for fault injection decisions
#[derive(Debug, Clone)]

pub enum FaultDecision {
    None,
    Latency {
        duration_ms: u64,
        rule_id: String,
    },
    Error {
        status: u16,
        body: String,
        rule_id: String,
        headers: std::collections::HashMap<String, String>,
    },
    /// Connection reset, requested via the v2 `reset()` result constructor (issue #357 Item 3) —
    /// the scripting analogue of the Mountebank-compatible `_rift.fault.tcp` / top-level `fault`
    /// carrier (see `imposter::fault_io::TcpFaultKind`). Callers apply it the same way: attach
    /// `TcpFaultKind::Reset` to a carrier response's extensions so the serve loop's `FaultIo`
    /// aborts the connection instead of framing a clean HTTP response.
    Reset {
        rule_id: String,
    },
}

/// Everything about the calling stub thread into `ctx.stub` (issue #357 Item 1). Fields are
/// `None` where unavailable, mirroring P1's `stub_id` contract (issue #355).
#[derive(Debug, Clone, Default)]
pub struct ScriptStubContext {
    pub scenario_name: Option<String>,
    pub scenario_state: Option<String>,
    pub stub_id: Option<String>,
}

/// The in-flight response, exposed as `ctx.response` on transform/decorate hooks only (issue
/// #357 Item 1). Absent (`None` on `ScriptCtxInput`) for every other hook point.
#[derive(Debug, Clone)]
pub struct ScriptResponseContext {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// Everything the shared `ctx` builder (issue #357 Item 1) needs, engine-agnostic. Each engine
/// (`rhai_engine`, `js_engine`) turns this into its own native `ctx` value; the
/// field names/semantics are identical across engines by contract — keep them that way.
#[derive(Debug, Clone)]
pub struct ScriptCtxInput<'a> {
    pub request: &'a ScriptRequest,
    pub response: Option<ScriptResponseContext>,
    pub flow_id: String,
    pub stub: ScriptStubContext,
    /// Imposter port, used only to tag `ctx.logger` output; 0 when not running under an imposter
    /// (e.g. the proxy path, or a bare `ScriptEngine::should_inject_fault` call in tests).
    pub port: u16,
}

impl<'a> ScriptCtxInput<'a> {
    pub fn new(request: &'a ScriptRequest, flow_id: impl Into<String>) -> Self {
        Self {
            request,
            response: None,
            flow_id: flow_id.into(),
            stub: ScriptStubContext::default(),
            port: 0,
        }
    }

    #[must_use]
    pub fn with_response(mut self, response: ScriptResponseContext) -> Self {
        self.response = Some(response);
        self
    }

    #[must_use]
    pub fn with_stub(mut self, stub: ScriptStubContext) -> Self {
        self.stub = stub;
        self
    }

    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// Extra, optional context a caller can thread into `ctx` beyond the bare `(request, flow_store)`
/// pair (issue #357 Item 1). Callers with no imposter context (the proxy path, direct engine unit
/// tests) use `Default`, which falls back to `request.headers["x-flow-id"]` (or `""`) for
/// `ctx.flowId` and leaves `ctx.stub` fields `None` — the real HTTP path
/// (`imposter::handler`) supplies the resolved flow id and stub metadata for full fidelity.
#[derive(Debug, Clone, Default)]
pub struct ScriptCtxExtras {
    pub flow_id: Option<String>,
    pub stub: ScriptStubContext,
    pub port: u16,
}

impl ScriptCtxExtras {
    pub fn build_ctx_input<'a>(&self, request: &'a ScriptRequest) -> ScriptCtxInput<'a> {
        let flow_id = self.flow_id.clone().unwrap_or_else(|| {
            request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-flow-id"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        });
        ScriptCtxInput::new(request, flow_id)
            .with_stub(self.stub.clone())
            .with_port(self.port)
    }
}

/// A `respond`/bare-expression return value that isn't a string or a JSON-shaped value (issue
/// #357 Item 3 — body values). `http(status, body)` decides which of these to serialize based on
/// the script value's own type: a string passes through verbatim; a map/array is JSON-serialized.
#[derive(Debug, Clone)]
pub enum ScriptResultBody {
    Json(Value),
    Str(String),
}

/// The v2 result constructors (issue #357 Item 3), engine-agnostic: `http()`/`delay()`/`reset()`/
/// `pass()` all build one of these, which `into_fault_decision` then turns into the
/// [`FaultDecision`] the caller applies. Each engine wraps this in its own native "builder" value
/// so `.header(k, v)` can chain.
#[derive(Debug, Clone)]
pub enum ScriptResult {
    /// `pass()` or a script returning nothing: respond normally, no injection.
    Pass,
    /// `delay(ms)`.
    Delay(u64),
    /// `reset()`: connection reset (the old `fault: "tcp"` shape).
    Reset,
    /// `http(status, body?)`, with zero or more `.header(k, v)` calls applied.
    Http {
        status: u16,
        body: Option<ScriptResultBody>,
        headers: Vec<(String, String)>,
    },
}

impl ScriptResult {
    pub fn http(status: u16, body: Option<ScriptResultBody>) -> Self {
        ScriptResult::Http {
            status,
            body,
            headers: Vec::new(),
        }
    }

    /// Apply a `.header(k, v)` builder call. A no-op on `Delay`/`Reset`/`Pass` — only `http()`
    /// results carry headers.
    pub fn add_header(&mut self, key: String, value: String) {
        if let ScriptResult::Http { headers, .. } = self {
            headers.push((key, value));
        }
    }

    /// Convert a result constructor (issue #357 Item 3) into the [`FaultDecision`] the caller
    /// applies. A JSON `body` gets `Content-Type: application/json` UNLESS the script already set
    /// a `content-type` header itself (case-insensitive match; an explicit header always wins).
    pub fn into_fault_decision(self, rule_id: &str) -> FaultDecision {
        match self {
            ScriptResult::Pass => FaultDecision::None,
            ScriptResult::Delay(duration_ms) => FaultDecision::Latency {
                duration_ms,
                rule_id: rule_id.to_string(),
            },
            ScriptResult::Reset => FaultDecision::Reset {
                rule_id: rule_id.to_string(),
            },
            ScriptResult::Http {
                status,
                body,
                headers,
            } => {
                let mut header_map: HashMap<String, String> = HashMap::new();
                let mut has_content_type = false;
                for (k, v) in headers {
                    if k.eq_ignore_ascii_case("content-type") {
                        has_content_type = true;
                    }
                    header_map.insert(k, v);
                }
                let body_str = match body {
                    None => String::new(),
                    Some(ScriptResultBody::Str(s)) => s,
                    Some(ScriptResultBody::Json(v)) => {
                        if !has_content_type {
                            header_map
                                .insert("Content-Type".to_string(), "application/json".to_string());
                        }
                        serde_json::to_string(&v).unwrap_or_default()
                    }
                };
                FaultDecision::Error {
                    status,
                    body: body_str,
                    rule_id: rule_id.to_string(),
                    headers: header_map,
                }
            }
        }
    }
}

/// Names of the v2 named entrypoints (issue #357 Items 2/4). Placement determines which name a
/// hook looks for (e.g. the `respond` hook calls a function named `respond`, if one is declared).
pub mod entrypoints {
    pub const RESPOND: &str = "respond";
    pub const MATCHES: &str = "matches";
    pub const TRANSFORM: &str = "transform";
    pub const DELAY: &str = "delay";
}

/// Unified script engine that supports Rhai and JavaScript
#[derive(Clone)]

pub enum ScriptEngine {
    Rhai(RhaiEngine),
    #[cfg(feature = "javascript")]
    JavaScript(JsEngine),
}

impl ScriptEngine {
    /// Create a new script engine based on the engine type
    pub fn new(engine_type: &str, script: &str, rule_id: &str) -> Result<Self> {
        match engine_type {
            "rhai" => Ok(ScriptEngine::Rhai(RhaiEngine::new(script, rule_id)?)),
            "lua" => Err(anyhow!(
                "the Lua scripting engine was removed (issue #450); use engine \"rhai\" or \"javascript\""
            )),
            #[cfg(feature = "javascript")]
            "javascript" | "js" => Ok(ScriptEngine::JavaScript(JsEngine::new(script, rule_id)?)),
            #[cfg(not(feature = "javascript"))]
            "javascript" | "js" => Err(anyhow!(
                "JavaScript engine is not enabled. Enable the 'javascript' feature flag"
            )),
            other => Err(anyhow!("Unknown script engine type: {other}")),
        }
    }

    /// Execute the `respond(ctx)` entrypoint (or bare-expression script) and determine if a fault
    /// should be injected. `ctx` gets best-effort defaults ([`ScriptCtxExtras::default`]) —
    /// callers with real flow-id/stub context should use [`Self::should_inject_fault_with_ctx`]
    /// instead.
    pub fn should_inject_fault(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        self.should_inject_fault_with_ctx(request, flow_store, &ScriptCtxExtras::default())
    }

    /// As [`Self::should_inject_fault`], but with real `ctx.flowId`/`ctx.stub` context (issue
    /// #357 Item 1) — used by the imposter `_rift.script` hook, which knows the resolved flow id
    /// and matched stub.
    pub fn should_inject_fault_with_ctx(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        extra: &ScriptCtxExtras,
    ) -> Result<FaultDecision> {
        match self {
            ScriptEngine::Rhai(engine) => {
                engine.should_inject_fault_with_ctx(request, flow_store, extra)
            }
            #[cfg(feature = "javascript")]
            ScriptEngine::JavaScript(engine) => {
                engine.should_inject_with_ctx(request, flow_store, extra)
            }
        }
    }
}

/// Request context passed to scripts
#[derive(Debug, Clone)]

pub struct ScriptRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
    /// Query parameters parsed from the URL
    pub query: HashMap<String, String>,
    /// Path parameters extracted from route patterns (e.g., /users/:id)
    pub path_params: HashMap<String, String>,
    /// The raw request body text, exactly as received (issue #357 Item 1: `ctx.request.body` is
    /// always the raw string, unifying the old split where the Mountebank path kept a raw string
    /// and the `_rift.script` path kept parsed JSON). `None` when the caller only has/derives a
    /// parsed `body`; ctx-building then falls back to re-serializing `body` (loses exact
    /// whitespace but not shape) so callers migrated ad hoc from `body` alone still work.
    pub raw_body: Option<String>,
    /// Whether `raw_body` is UTF-8 text or a base64-encoded binary body (issue #636). Mirrors
    /// `ResponseMode`/`_mode` on the response side so scripts can tell which they got instead of
    /// silently treating base64 as text.
    pub mode: ResponseMode,
}

/// Wrapper for FlowStore that can be used in scripts (Rhai and JavaScript)
/// Uses direct synchronous calls since FlowStore is no longer async
///
/// Every op is unconditionally fail-loud (issues #322/#358): a backend failure always raises a
/// script error, so a store outage is never conflated with "absent"/"false"/"0".
#[derive(Clone)]
pub struct ScriptFlowStore {
    store: Arc<dyn FlowStore>,
}

impl ScriptFlowStore {
    /// The `ctx.state` handle.
    pub fn new(store: Arc<dyn FlowStore>) -> Self {
        Self { store }
    }

    /// Get a value from flow state. Raises on a backend failure.
    pub fn get(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<Dynamic, Box<rhai::EvalAltResult>> {
        match flow_result("get", self.store.get(&flow_id, &key)) {
            Ok(Some(val)) => Ok(rhai_engine::json_to_dynamic(val)),
            Ok(None) => Ok(Dynamic::UNIT),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Set a value in flow state. Raises on a backend failure.
    pub fn set(
        &mut self,
        flow_id: String,
        key: String,
        value: Dynamic,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        let json_val = rhai_engine::dynamic_to_json(value);
        match flow_result(
            "set",
            self.store.set(&flow_id, &key, json_val).map(|()| true),
        ) {
            Ok(v) => Ok(v),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Check if a key exists. Raises on a backend failure.
    pub fn exists(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        match flow_result("exists", self.store.exists(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Delete a key. Raises on a backend failure.
    pub fn delete(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        match flow_result("delete", self.store.delete(&flow_id, &key).map(|()| true)) {
            Ok(v) => Ok(v),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Increment a counter. Raises on a backend failure.
    pub fn increment(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<i64, Box<rhai::EvalAltResult>> {
        match flow_result("increment", self.store.increment(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Set TTL for a flow. Raises on a backend failure.
    pub fn set_ttl(
        &mut self,
        flow_id: String,
        ttl_seconds: i64,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        match flow_result(
            "setTtl",
            self.store.set_ttl(&flow_id, ttl_seconds).map(|()| true),
        ) {
            Ok(v) => Ok(v),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Take (read and clear) the last flow-store op error for this thread, or unit if the
    /// last op succeeded (issue #322).
    pub fn last_error(&mut self) -> Dynamic {
        match crate::extensions::flow_state::take_last_flow_error() {
            Some(msg) => Dynamic::from(msg),
            None => Dynamic::UNIT,
        }
    }

    // ============================================================
    // Atomic ops + ergonomic getters. Like the ops above, these ALWAYS raise a script error on a
    // backend failure — a store outage must never be conflated with "key absent"/"conflict".
    // ============================================================

    /// Get a value, or `default` if the key is absent. A store failure always raises.
    pub fn get_or(
        &mut self,
        flow_id: String,
        key: String,
        default: Dynamic,
    ) -> std::result::Result<Dynamic, Box<rhai::EvalAltResult>> {
        match flow_result("getOr", self.store.get(&flow_id, &key)) {
            Ok(Some(val)) => Ok(rhai_engine::json_to_dynamic(val)),
            Ok(None) => Ok(default),
            Err(msg) => Err(msg.into()),
        }
    }

    /// Atomically increment by `by`, starting at 0 when absent. Always fail-loud.
    pub fn increment_by(
        &mut self,
        flow_id: String,
        key: String,
        by: i64,
    ) -> std::result::Result<i64, Box<rhai::EvalAltResult>> {
        flow_result("incrementBy", self.store.increment_by(&flow_id, &key, by)).map_err(Into::into)
    }

    /// Atomic compare-and-set (issues #358, #311): `key` is set to `new` iff its current value
    /// equals `expected` (unit means "not present"). Returns the raw [`CasOutcome`]; the caller
    /// (the Rhai-specific `RhaiStateHandle`) converts it to the engine's object-map return shape.
    /// Always fail-loud.
    pub fn cas(
        &mut self,
        flow_id: String,
        key: String,
        expected: Dynamic,
        new: Dynamic,
    ) -> std::result::Result<CasOutcome, Box<rhai::EvalAltResult>> {
        let expected_json = if expected.is_unit() {
            None
        } else {
            Some(rhai_engine::dynamic_to_json(expected))
        };
        let new_json = rhai_engine::dynamic_to_json(new);
        flow_result(
            "cas",
            self.store
                .compare_and_set(&flow_id, &key, expected_json.as_ref(), new_json),
        )
        .map_err(Into::into)
    }

    /// Set a per-flow TTL override. Always fail-loud.
    pub fn ttl(
        &mut self,
        flow_id: String,
        ttl_seconds: i64,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        flow_result(
            "ttl",
            self.store.set_ttl(&flow_id, ttl_seconds).map(|()| true),
        )
        .map_err(Into::into)
    }

    /// Set a per-key TTL override (issue #530). Returns `true` if the key existed, `false` if
    /// absent; `ttl_seconds <= 0` deletes the key. Always fail-loud.
    pub fn key_ttl(
        &mut self,
        flow_id: String,
        key: String,
        ttl_seconds: i64,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        flow_result(
            "keyTtl",
            self.store.set_key_ttl(&flow_id, &key, ttl_seconds),
        )
        .map_err(Into::into)
    }

    /// Remove every key in a flow (issue #530). Returns `true`. Always fail-loud.
    pub fn clear(
        &mut self,
        flow_id: String,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        flow_result("clear", self.store.clear_flow(&flow_id).map(|()| true)).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::flow_state::NoOpFlowStore;
    use serde_json::json;
    use std::collections::HashMap;

    /// A flow store whose every op fails — for exercising the error branches of the wrappers.
    struct FailingStore;
    impl FlowStore for FailingStore {
        fn get(&self, _: &str, _: &str) -> Result<Option<Value>> {
            Err(anyhow!("boom"))
        }
        fn set(&self, _: &str, _: &str, _: Value) -> Result<()> {
            Err(anyhow!("boom"))
        }
        fn exists(&self, _: &str, _: &str) -> Result<bool> {
            Err(anyhow!("boom"))
        }
        fn delete(&self, _: &str, _: &str) -> Result<()> {
            Err(anyhow!("boom"))
        }
        fn increment(&self, _: &str, _: &str) -> Result<i64> {
            Err(anyhow!("boom"))
        }
        fn set_ttl(&self, _: &str, _: i64) -> Result<()> {
            Err(anyhow!("boom"))
        }
        fn set_key_ttl(&self, _: &str, _: &str, _: i64) -> Result<bool> {
            Err(anyhow!("boom"))
        }
        fn clear_flow(&self, _: &str) -> Result<()> {
            Err(anyhow!("boom"))
        }
    }

    // Every ScriptFlowStore op, on a backend failure, raises a script error AND records the
    // error for `last_error()` (issue #322). Covers all six ops — a wrong result in any one
    // would be caught here.
    #[test]
    fn rhai_flow_store_ops_fail_loud_and_record_error() {
        use crate::extensions::flow_state::take_last_flow_error;
        let mut s = ScriptFlowStore::new(Arc::new(FailingStore));

        let _ = take_last_flow_error();
        assert!(
            s.get("f".into(), "k".into()).is_err(),
            "get must raise on a backend failure"
        );
        assert!(
            take_last_flow_error().is_some_and(|e| e.contains("get")),
            "get records last_error"
        );

        assert!(
            s.set("f".into(), "k".into(), Dynamic::from(1)).is_err(),
            "set must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("set")));

        assert!(
            s.exists("f".into(), "k".into()).is_err(),
            "exists must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("exists")));

        assert!(
            s.delete("f".into(), "k".into()).is_err(),
            "delete must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("delete")));

        assert!(
            s.increment("f".into(), "k".into()).is_err(),
            "increment must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("increment")));

        assert!(
            s.set_ttl("f".into(), 60).is_err(),
            "set_ttl must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("setTtl")));

        // Issue #530: the per-key ttl and clear ops are fail-loud too.
        assert!(
            s.key_ttl("f".into(), "k".into(), 60).is_err(),
            "key_ttl must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("keyTtl")));

        assert!(
            s.clear("f".into()).is_err(),
            "clear must raise on a backend failure"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("clear")));
    }

    // AC2 (issue #322): the Rhai flow_store.last_error() accessor surfaces the last recorded
    // backend error (then leaves the slot taken so a following op reflects its own status).
    #[test]
    fn rhai_flow_store_last_error_surfaces() {
        use crate::extensions::flow_state::{log_flow_err, take_last_flow_error};
        let _ = take_last_flow_error();
        let mut s = ScriptFlowStore::new(Arc::new(NoOpFlowStore));
        // Simulate a failed op recording an error through the shared seam.
        let _ = log_flow_err(
            "increment",
            0i64,
            Err::<i64, _>(anyhow::anyhow!("backend down")),
        );
        let err = s.last_error();
        assert!(
            err.into_string()
                .map(|x| x.contains("backend down"))
                .unwrap_or(false),
            "last_error() must surface the recorded backend error"
        );
        // last_error() took the value: a subsequent call with no new failure returns unit.
        assert!(s.last_error().is_unit());
    }

    // ============================================
    // Tests for FaultDecision enum
    // ============================================

    #[test]
    fn test_fault_decision_none() {
        let decision = FaultDecision::None;
        match decision {
            FaultDecision::None => {}
            _ => panic!("Expected FaultDecision::None"),
        }
    }

    #[test]
    fn test_fault_decision_latency() {
        let decision = FaultDecision::Latency {
            duration_ms: 500,
            rule_id: "test-rule".to_string(),
        };
        match decision {
            FaultDecision::Latency {
                duration_ms,
                rule_id,
            } => {
                assert_eq!(duration_ms, 500);
                assert_eq!(rule_id, "test-rule");
            }
            _ => panic!("Expected FaultDecision::Latency"),
        }
    }

    #[test]
    fn test_fault_decision_error() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());

        let decision = FaultDecision::Error {
            status: 503,
            body: r#"{"error": "service unavailable"}"#.to_string(),
            rule_id: "error-rule".to_string(),
            headers,
        };
        match decision {
            FaultDecision::Error {
                status,
                body,
                rule_id,
                headers,
            } => {
                assert_eq!(status, 503);
                assert!(body.contains("service unavailable"));
                assert_eq!(rule_id, "error-rule");
                assert_eq!(headers.get("X-Custom"), Some(&"value".to_string()));
            }
            _ => panic!("Expected FaultDecision::Error"),
        }
    }

    #[test]
    fn test_fault_decision_clone() {
        let decision = FaultDecision::Latency {
            duration_ms: 100,
            rule_id: "clone-test".to_string(),
        };
        let cloned = decision.clone();
        match cloned {
            FaultDecision::Latency {
                duration_ms,
                rule_id,
            } => {
                assert_eq!(duration_ms, 100);
                assert_eq!(rule_id, "clone-test");
            }
            _ => panic!("Expected cloned FaultDecision::Latency"),
        }
    }

    #[test]
    fn test_fault_decision_debug() {
        let decision = FaultDecision::None;
        let debug_str = format!("{decision:?}");
        assert!(debug_str.contains("None"));
    }

    // Issue #357 Item 3: `reset()` maps to FaultDecision::Reset.
    #[test]
    fn test_fault_decision_reset() {
        let decision = FaultDecision::Reset {
            rule_id: "reset-rule".to_string(),
        };
        match decision {
            FaultDecision::Reset { rule_id } => assert_eq!(rule_id, "reset-rule"),
            _ => panic!("Expected FaultDecision::Reset"),
        }
    }

    // Issue #357 Item 3: the ScriptResult -> FaultDecision conversions used by every engine.
    #[test]
    fn test_script_result_into_fault_decision() {
        assert!(matches!(
            ScriptResult::Pass.into_fault_decision("r"),
            FaultDecision::None
        ));
        assert!(matches!(
            ScriptResult::Delay(42).into_fault_decision("r"),
            FaultDecision::Latency {
                duration_ms: 42,
                ..
            }
        ));
        assert!(matches!(
            ScriptResult::Reset.into_fault_decision("r"),
            FaultDecision::Reset { .. }
        ));

        let mut json_result =
            ScriptResult::http(503, Some(ScriptResultBody::Json(json!({"e": 1}))));
        json_result.add_header("Retry-After".to_string(), "1".to_string());
        match json_result.into_fault_decision("r") {
            FaultDecision::Error {
                status,
                body,
                headers,
                ..
            } => {
                assert_eq!(status, 503);
                assert_eq!(body, r#"{"e":1}"#);
                assert_eq!(headers.get("Retry-After").map(String::as_str), Some("1"));
                assert_eq!(
                    headers.get("Content-Type").map(String::as_str),
                    Some("application/json")
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // An explicit content-type header always wins over the JSON-body default.
        let mut custom_ct = ScriptResult::http(200, Some(ScriptResultBody::Json(json!({}))));
        custom_ct.add_header("content-type".to_string(), "application/custom".to_string());
        match custom_ct.into_fault_decision("r") {
            FaultDecision::Error { headers, .. } => {
                assert_eq!(
                    headers.get("content-type").map(String::as_str),
                    Some("application/custom")
                );
                assert!(!headers.contains_key("Content-Type"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // A string body passes through verbatim with no Content-Type added.
        match ScriptResult::http(200, Some(ScriptResultBody::Str("hi".to_string())))
            .into_fault_decision("r")
        {
            FaultDecision::Error { body, headers, .. } => {
                assert_eq!(body, "hi");
                assert!(!headers.contains_key("Content-Type"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ============================================
    // Tests for ScriptRequest
    // ============================================

    #[test]
    fn test_script_request_creation() {
        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "POST".to_string(),
            path: "/api/users".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!({"name": "test"}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/users");
    }

    #[test]
    fn test_script_request_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("Authorization".to_string(), "Bearer token".to_string());

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/data".to_string(),
            headers,
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        assert_eq!(request.headers.len(), 2);
        assert_eq!(
            request.headers.get("Content-Type"),
            Some(&"application/json".to_string())
        );
    }

    #[test]
    fn test_script_request_with_query_params() {
        let mut query = HashMap::new();
        query.insert("page".to_string(), "1".to_string());
        query.insert("limit".to_string(), "10".to_string());

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/items".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query,
            path_params: HashMap::new(),
        };
        assert_eq!(request.query.get("page"), Some(&"1".to_string()));
        assert_eq!(request.query.get("limit"), Some(&"10".to_string()));
    }

    #[test]
    fn test_script_request_with_path_params() {
        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "123".to_string());
        path_params.insert("action".to_string(), "edit".to_string());

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "PUT".to_string(),
            path: "/api/users/123/edit".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!({"name": "updated"}),
            query: HashMap::new(),
            path_params,
        };
        assert_eq!(request.path_params.get("id"), Some(&"123".to_string()));
    }

    #[test]
    fn test_script_request_clone() {
        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "DELETE".to_string(),
            path: "/api/items/456".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let cloned = request.clone();
        assert_eq!(cloned.method, "DELETE");
        assert_eq!(cloned.path, "/api/items/456");
    }

    #[test]
    fn test_script_request_debug() {
        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let debug_str = format!("{request:?}");
        assert!(debug_str.contains("GET"));
        assert!(debug_str.contains("/test"));
    }

    // ============================================
    // Tests for ScriptFlowStore
    // ============================================

    #[test]
    fn test_script_flow_store_creation() {
        let store = Arc::new(NoOpFlowStore);
        let script_store = ScriptFlowStore::new(store);
        assert!(std::mem::size_of_val(&script_store) > 0);
    }

    #[test]
    fn test_script_flow_store_get() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store
            .get("flow-1".to_string(), "key".to_string())
            .expect("NoOp get never fails");
        // NoOpFlowStore returns Unit for get
        assert!(result.is_unit());
    }

    #[test]
    fn test_script_flow_store_set() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store
            .set(
                "flow-1".to_string(),
                "key".to_string(),
                rhai::Dynamic::from(42),
            )
            .expect("NoOp set never fails");
        assert!(result);
    }

    #[test]
    fn test_script_flow_store_exists() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store
            .exists("flow-1".to_string(), "key".to_string())
            .expect("NoOp exists never fails");
        assert!(!result); // NoOpFlowStore always returns false
    }

    #[test]
    fn test_script_flow_store_delete() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store
            .delete("flow-1".to_string(), "key".to_string())
            .expect("NoOp delete never fails");
        assert!(result);
    }

    #[test]
    fn test_script_flow_store_increment() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        // NoOpFlowStore returns 0 on error (which doesn't happen, but increment returns 1)
        let result = script_store
            .increment("flow-1".to_string(), "counter".to_string())
            .expect("NoOp increment never fails");
        assert_eq!(result, 1);
    }

    #[test]
    fn test_script_flow_store_set_ttl() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store
            .set_ttl("flow-1".to_string(), 3600)
            .expect("NoOp set_ttl never fails");
        assert!(result);
    }

    #[test]
    fn test_script_flow_store_clone() {
        let store = Arc::new(NoOpFlowStore);
        let script_store = ScriptFlowStore::new(store);
        let cloned = script_store.clone();
        // Both should reference the same underlying store
        assert!(std::mem::size_of_val(&cloned) > 0);
    }

    // ============================================
    // Tests for ScriptEngine enum
    // ============================================

    #[test]
    fn test_script_engine_new_rhai() {
        let script = r#"
            fn respond(ctx) {
                pass()
            }
        "#;
        let engine = ScriptEngine::new("rhai", script, "test-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_script_engine_new_invalid_type() {
        let script = "return false";
        let engine = ScriptEngine::new("invalid_engine", script, "test-rule");
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("Unknown script engine type"));
    }

    #[test]
    fn test_script_engine_rhai_execution() {
        let script = r#"
            fn respond(ctx) {
                pass()
            }
        "#;
        let engine = ScriptEngine::new("rhai", script, "test-rule").unwrap();

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let flow_store: Arc<dyn crate::extensions::flow_state::FlowStore> = Arc::new(NoOpFlowStore);
        let result = engine.should_inject_fault(&request, flow_store);
        assert!(result.is_ok());
    }

    // ============================================
    // Tests for RhaiEngine creation
    // ============================================

    #[test]
    fn test_rhai_engine_creation_valid_script() {
        let script = r#"
            fn respond(ctx) {
                pass()
            }
        "#;
        let engine = RhaiEngine::new(script, "valid-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_rhai_engine_creation_syntax_error() {
        let script = r#"
            fn respond(ctx {  // Missing closing paren
                return false;
            }
        "#;
        let engine = RhaiEngine::new(script, "invalid-rule");
        assert!(engine.is_err());
    }

    #[test]
    fn test_rhai_engine_ast_access() {
        let script = r#"
            fn respond(ctx) {
                pass()
            }
        "#;
        let engine = RhaiEngine::new(script, "test-rule").unwrap();
        let ast = engine.ast();
        assert!(std::mem::size_of_val(ast) > 0);
    }

    // ============================================
    // Feature-gated tests
    // ============================================

    // Issue #450: Lua was removed; "lua" now always fails with an actionable error pointing at
    // the two remaining engines, rather than a feature-flag message.
    #[test]
    fn test_script_engine_new_lua_removed() {
        let engine = ScriptEngine::new("lua", "return false", "test-rule");
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("removed"), "unexpected message: {err_msg}");
        assert!(err_msg.contains("rhai"));
        assert!(err_msg.contains("javascript"));
    }

    #[cfg(feature = "javascript")]
    mod js_tests {
        use super::*;

        #[test]
        fn test_script_engine_new_javascript() {
            let script = r#"
                function respond(ctx) {
                    return pass();
                }
            "#;
            let engine = ScriptEngine::new("javascript", script, "js-rule");
            assert!(
                engine.is_ok(),
                "JS engine creation failed: {:?}",
                engine.err()
            );
        }

        #[test]
        fn test_script_engine_new_js_alias() {
            let script = r#"
                function respond(ctx) {
                    return pass();
                }
            "#;
            let engine = ScriptEngine::new("js", script, "js-rule");
            assert!(
                engine.is_ok(),
                "JS engine creation failed: {:?}",
                engine.err()
            );
        }

        #[test]
        fn test_compile_js_to_bytecode() {
            let script = r#"
                function respond(ctx) {
                    return pass();
                }
            "#;
            let bytecode = super::super::compile_js_to_bytecode(script);
            assert!(bytecode.is_ok());
        }
    }

    // ============================================
    // Tests for disabled features
    // ============================================

    #[cfg(not(feature = "javascript"))]
    #[test]
    fn test_javascript_engine_disabled() {
        let engine = ScriptEngine::new("javascript", "return false", "test");
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("not enabled") || err_msg.contains("feature"));
    }
}
