use crate::extensions::flow_state::{CasOutcome, FlowStore, flow_result};
use crate::scripting::{
    FaultDecision, ScriptCtxExtras, ScriptCtxInput, ScriptRequest, ScriptResponseContext,
    ScriptResult, ScriptResultBody, ScriptStubContext, entrypoints,
};
use anyhow::{Result, anyhow};
use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsValue, Source, js_string,
    native_function::NativeFunction, object::builtins::JsArray, property::PropertyKey,
};

/// Create a JavaScript object with proper Object.prototype
/// This ensures the object has toString, valueOf, etc. for proper JS operations
fn create_js_object(context: &Context) -> JsObject {
    JsObject::with_object_proto(context.intrinsics())
}
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::sync::Arc;

// Thread-local storage for the flow store during script execution
thread_local! {
    static CURRENT_FLOW_STORE: RefCell<Option<Arc<dyn FlowStore>>> = const { RefCell::new(None) };
}

// Thread-local registry backing the v2 result-constructor values (`http`/`delay`/`reset`/`pass`,
// issue #357 Item 3). Boa's `JsObject` is a plain property bag with no easy way to embed an
// opaque Rust value, so each constructor stashes its [`ScriptResult`] here under a fresh id and
// hands the script a `{ __riftResultId: id }` object; `.header()` and the final conversion look
// the value back up by id (mirroring the `CURRENT_FLOW_STORE` thread-local pattern above).
thread_local! {
    static SCRIPT_RESULT_REGISTRY: RefCell<std::collections::HashMap<u64, ScriptResult>> =
        RefCell::new(std::collections::HashMap::new());
    static SCRIPT_RESULT_NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Set the current flow store for thread-local access
fn set_current_flow_store(store: Arc<dyn FlowStore>) {
    CURRENT_FLOW_STORE.with(|s| {
        *s.borrow_mut() = Some(store);
    });
}

/// Clear the current flow store
fn clear_current_flow_store() {
    CURRENT_FLOW_STORE.with(|s| {
        *s.borrow_mut() = None;
    });
}

/// Get the current flow store
fn with_current_flow_store<T>(f: impl FnOnce(&Arc<dyn FlowStore>) -> T) -> Option<T> {
    CURRENT_FLOW_STORE.with(|s| s.borrow().as_ref().map(f))
}

/// JavaScript script engine for fault injection using Boa Engine
///
/// # Script Interface (v2 `ctx` API, issue #357; the pre-1.0 v1 `should_inject` contract was
/// removed in issue #453)
///
/// Scripts define a `respond` function (or a bare expression with no function declarations at
/// all) taking a single `ctx` argument:
///
/// ```javascript
/// function respond(ctx) {
///     // Your logic here
///     return pass();
/// }
/// ```
///
/// ## `ctx.request`
///
/// - `ctx.request.method` - HTTP method (string): "GET", "POST", "PUT", "DELETE", etc.
/// - `ctx.request.path` - Request path (string): "/api/users/123"
/// - `ctx.request.headers` - Object of (lowercased) header name to value
/// - `ctx.request.header(name)` - Case-insensitive header getter
/// - `ctx.request.body` - Raw request body (string)
/// - `ctx.request.json` - Lazily parsed JSON body (null if not valid JSON)
/// - `ctx.request.query` - Object of query parameter name to value
/// - `ctx.request.pathParams` - Object of path parameters extracted from route patterns
///
/// ## `ctx.state` (flow-scoped storage)
///
/// `ctx.state` is already scoped to the current flow id:
/// - `ctx.state.get(key)` - Get a stored value (returns null if not found)
/// - `ctx.state.set(key, value)` - Store a value (returns boolean)
/// - `ctx.state.exists(key)` - Check if key exists (returns boolean)
/// - `ctx.state.delete(key)` - Delete a key (returns boolean)
/// - `ctx.state.incr(key)` - Increment counter (returns number)
/// - `ctx.state.getOr(key, default)` - Get a value, or `default` if absent
/// - `ctx.state.incrBy(key, n)` - Atomic increment by `n`, starting at 0 when absent
/// - `ctx.state.cas(key, expected, new)` - Atomic compare-and-set
/// - `ctx.state.ttl(seconds)` - Set flow expiration
///
/// `ctx.store.flow(flowId)` returns the same handle scoped to an arbitrary (not necessarily the
/// request's own) flow id.
///
/// ## Return Value
///
/// The function returns one of the result constructors, or nothing (equivalent to `pass()`):
///
/// ```javascript
/// // No fault injection
/// pass()
///
/// // Latency injection
/// delay(500)
///
/// // Error injection
/// http(503, "Service unavailable")
///
/// // Error with custom headers and a JSON body
/// http(429, { error: "Rate limited" }).header("Retry-After", "60")
/// ```
///
/// ## Example
///
/// ```javascript
/// function respond(ctx) {
///     // Rate limit based on the resolved flow id
///     var attempts = ctx.state.incr("attempts");
///     if (attempts > 3) {
///         return http(429, "Rate limited");
///     }
///
///     // Inject fault for POST requests to specific path
///     if (ctx.request.method === "POST" && ctx.request.path === "/api/test") {
///         return delay(100);
///     }
///
///     return pass();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct JsEngine {
    script: String,
    rule_id: String,
}

impl JsEngine {
    pub fn new(script: &str, rule_id: &str) -> Result<Self> {
        // Validate the script PARSES — not that it runs. Issue #357 Item 2 legalizes bare-
        // expression scripts, which reference a top-level `ctx` that only exists at real
        // execution time; fully evaluating an unbound bare script here would spuriously fail with
        // "ctx is not defined" and, for a wrapper-form script, would run its top-level side
        // effects at construction time, not request time. `Script::parse` catches genuine syntax
        // errors without executing anything.
        let mut context = Context::default();
        boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut context)
            .map_err(|e| anyhow!("Failed to compile JavaScript script: {e}"))?;

        Ok(Self {
            script: script.to_string(),
            rule_id: rule_id.to_string(),
        })
    }

    /// Execute the `respond(ctx)` entrypoint (or bare-expression script) and determine if a fault
    /// should be injected — see
    /// [`crate::scripting::ScriptEngine::should_inject_fault`] for the full contract.
    pub fn should_inject(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        self.should_inject_with_ctx(request, flow_store, &ScriptCtxExtras::default())
    }

    /// As [`Self::should_inject`], but with real `ctx.flowId`/`ctx.stub` context (issue #357
    /// Item 1).
    pub fn should_inject_with_ctx(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        extra: &ScriptCtxExtras,
    ) -> Result<FaultDecision> {
        let ctx_input = extra.build_ctx_input(request);
        execute_js_script(&self.script, flow_store, &self.rule_id, &ctx_input)
    }
}

/// Compile JavaScript to bytecode for caching
/// Returns serialized bytecode that can be loaded efficiently
/// Note: Boa doesn't support bytecode serialization yet, so we store the source
/// and validate it compiles
// Used by proxy.rs but cross-module analysis doesn't see it
pub fn compile_js_to_bytecode(script: &str) -> Result<Vec<u8>> {
    // Validate the script PARSES (not that it runs) — see the comment on `JsEngine::new` for why
    // a bare v2 script can't be blind-evaluated here (issue #357 Items 2/4).
    let mut context = Context::default();
    boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut context)
        .map_err(|e| anyhow!("Failed to compile JavaScript script: {e}"))?;

    // Store the source as "bytecode" since Boa doesn't support
    // serialized bytecode yet
    Ok(script.as_bytes().to_vec())
}

/// Execute JavaScript with a fresh context
/// This is used by the script pool workers
pub fn execute_js_bytecode(
    bytecode: &[u8],
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let script =
        std::str::from_utf8(bytecode).map_err(|e| anyhow!("Invalid UTF-8 in bytecode: {e}"))?;
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(request);
    execute_js_script(script, flow_store, rule_id, &ctx_input)
}

/// Internal function to execute JavaScript script
fn execute_js_script(
    script: &str,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
) -> Result<FaultDecision> {
    // Set the flow store in thread-local storage for native functions to access
    set_current_flow_store(flow_store);

    // Ensure we clear the flow store when done (even on error)
    let result = execute_js_script_inner(script, rule_id, ctx_input);

    clear_current_flow_store();

    result
}

/// Loop-iteration cap for a `respond(ctx)` Boa context (issue #327). Boa exposes no
/// per-instruction interrupt, so a runaway loop (`while(true){}`) can't observe the deadline abort
/// flag and would otherwise run its `spawn_blocking` thread forever. This cap makes a single
/// runaway loop throw once the limit is hit, so the execution returns `Err` and the thread is
/// freed. Generous enough that no realistic boolean fault-decision script approaches it.
///
/// Known limitation (issue #371): Boa's loop-iteration counter is **per-call-frame** (reset to 0 on
/// every function call), and Boa 0.20 has no cumulative-work budget or per-instruction interrupt.
/// So a *nested* runaway — e.g. `while (true) { f(); }` where `f` loops — amplifies by roughly
/// `limit^depth` before the outermost loop trips, occupying its thread far longer than a flat
/// `while(true){}`. This is bounded, not infinite, and the client is still released at the
/// wall-clock timeout with a 500; only a background thread lingers, and only for a deliberately
/// adversarial *trusted* (operator-authored) script. Deep recursion is separately bounded by Boa's
/// default `recursion_limit` (512). Fully closing the loop-amplification would need a Boa fuel /
/// interrupt mechanism that 0.20 lacks.
const JS_SCRIPT_LOOP_ITERATION_LIMIT: u64 = 10_000_000;

/// Recursion depth cap applied to every bounded Boa `Context` (issue #355 Item 3). Boa's own
/// default is already 512; set explicitly so the contract doesn't silently drift if Boa's default
/// ever changes.
const JS_SCRIPT_RECURSION_LIMIT: usize = 512;

/// Stack size cap (bytes) applied to every bounded Boa `Context` (issue #355 Item 3), mirroring
/// Boa's own default (10 * 1024).
const JS_SCRIPT_STACK_SIZE_LIMIT: usize = 10 * 1024;

/// Build a `Context` with the interpreter-level guards every Mountebank JS hook (inject response,
/// predicate inject, `predicateGenerators.inject`, decorate) must run under (issue #355 Items 2/3).
///
/// Boa 0.20 has no per-instruction wall-clock interrupt, so these MB hooks — which execute
/// synchronously, inline in the request path, with no `spawn_blocking` wrapper — can't honor a
/// wall-clock deadline the way `_rift.script` does (see `bounded.rs::should_inject_bounded`).
/// The loop-iteration cap is what makes a `while(true){}` throw instead of hanging its thread; the
/// recursion/stack-size limits catch runaway recursion the same way. This is the enforcement layer
/// that stands in for the `_rift.script` wall-clock budget (`bounded::DEFAULT_SCRIPT_TIMEOUT_MS`)
/// for these sync call sites — every one of them must build its `Context` through this helper
/// rather than `Context::default()`.
pub(crate) fn bounded_js_context() -> Context {
    let mut context = Context::default();
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(JS_SCRIPT_LOOP_ITERATION_LIMIT);
    limits.set_recursion_limit(JS_SCRIPT_RECURSION_LIMIT);
    limits.set_stack_size_limit(JS_SCRIPT_STACK_SIZE_LIMIT);
    context
}

/// The top-level function names a JS script declares, for the static entrypoint check (issue
/// #360 Item 1). Two-phase, deliberately NOT a blind `context.eval` of the raw source:
///
///  1. **Syntax** — `Script::parse` catches genuine syntax errors WITHOUT executing anything
///     (exactly what [`JsEngine::new`] does). `Err` here is a real syntax error.
///  2. **Detection** — evaluate once with the SAME host globals the real execution path binds
///     (`ctx`/`http`/`delay`/`reset`/`pass`), under the SAME loop-iteration limit
///     ([`JS_SCRIPT_LOOP_ITERATION_LIMIT`]) that stops a top-level `while(true){}` from hanging.
///     Binding the globals is what keeps a legitimate #357 bare-expression response script (e.g.
///     `http(503, "boom")`, or `(ctx.request.method === "POST") ? http(503) : pass()`) from
///     spuriously throwing "http/ctx is not defined" — the very false-fail a blind unbound eval
///     caused. The eval's own Result is intentionally ignored: JS hoists every top-level
///     `function` declaration onto the global object BEFORE running statements, so the
///     declared-function set is complete even if a bare expression later throws at runtime — a
///     runtime throw is not this check's concern (matching Rhai, which never executes at all).
///
/// Returns the names of functions the SCRIPT itself declared (via a before/after global-key
/// diff), never the host builtins/globals bound above.
pub(crate) fn declared_functions_js(script: &str) -> Result<Vec<String>> {
    // Phase 1: syntax only.
    {
        let mut parse_ctx = Context::default();
        boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut parse_ctx)
            .map_err(|e| anyhow!("Syntax error: {e}"))?;
    }

    // Phase 2: bounded eval with real globals, then diff for declared functions.
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());

    let dummy_request = ScriptRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: std::collections::HashMap::new(),
        body: Value::Null,
        query: std::collections::HashMap::new(),
        path_params: std::collections::HashMap::new(),
        raw_body: None,
    };
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(&dummy_request);
    // A top-level `ctx.state.*` call needs a flow store bound; a no-op one is enough — the check
    // never inspects state, only which functions the script declared.
    set_current_flow_store(Arc::new(crate::extensions::flow_state::NoOpFlowStore));

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(JS_SCRIPT_LOOP_ITERATION_LIMIT);
    context
        .runtime_limits_mut()
        .set_recursion_limit(JS_SCRIPT_RECURSION_LIMIT);
    context
        .runtime_limits_mut()
        .set_stack_size_limit(JS_SCRIPT_STACK_SIZE_LIMIT);

    let ctx_obj = create_ctx_object(&mut context, &ctx_input)?;
    register_result_constructors(&mut context)?;

    let global = context.global_object();
    global
        .set(js_string!("ctx"), ctx_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set ctx global: {e}"))?;

    let pre_existing: std::collections::HashSet<String> = global
        .own_property_keys(&mut context)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| match k {
            PropertyKey::String(s) => Some(s.to_std_string_escaped()),
            _ => None,
        })
        .collect();

    // Ignore the eval Result: a bare-expression script may throw at runtime (that's not this
    // check's job), while hoisted function declarations are already on the global regardless.
    let _ = context.eval(Source::from_bytes(script.as_bytes()));

    let mut names = Vec::new();
    for key in global.own_property_keys(&mut context).unwrap_or_default() {
        let name = match &key {
            PropertyKey::String(s) => s.to_std_string_escaped(),
            _ => continue,
        };
        if pre_existing.contains(&name) {
            continue;
        }
        if global
            .get(key, &mut context)
            .ok()
            .filter(JsValue::is_callable)
            .is_some()
        {
            names.push(name);
        }
    }
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());
    Ok(names)
}

/// Inner function that does the actual JavaScript execution
fn execute_js_script_inner(
    script: &str,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
) -> Result<FaultDecision> {
    execute_js_script_inner_bounded(script, rule_id, ctx_input, JS_SCRIPT_LOOP_ITERATION_LIMIT)
}

/// As [`execute_js_script_inner`], but with an explicit loop-iteration cap so tests can bound a
/// runaway cheaply (issue #327).
///
/// Dispatch (issue #357 Item 2): the script is evaluated once with `ctx` already set — its
/// completion value is the bare-expression result. Then: a global `respond` → named entrypoint
/// (call it with `ctx`). Else → bare (the completion value from the single eval above).
fn execute_js_script_inner_bounded(
    script: &str,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
    loop_iteration_limit: u64,
) -> Result<FaultDecision> {
    // B3 (issue #357): clear any `ScriptResult` registry entries left over from a previous run on
    // this reused `spawn_blocking` worker thread. Only the single RETURNED result's id is removed
    // in `js_value_to_fault_decision`; constructor calls that aren't the returned value (a
    // top-level `http(500)` completion, `var a = http(500); return pass();`, etc.) would otherwise
    // leak an entry forever. Reset per execution, mirroring the per-task flow-store reset.
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(loop_iteration_limit);

    let ctx_obj = create_ctx_object(&mut context, ctx_input)?;
    register_result_constructors(&mut context)?;

    let global = context.global_object();
    global
        .set(js_string!("ctx"), ctx_obj.clone(), false, &mut context)
        .map_err(|e| anyhow!("Failed to set ctx global: {e}"))?;

    // Snapshot the global own-property names present BEFORE eval, so afterwards we can tell which
    // function globals the SCRIPT itself declared (B1): Boa builtins plus the
    // ctx/http/delay/reset/pass globals we set above are all already present here.
    let pre_existing: std::collections::HashSet<String> = global
        .own_property_keys(&mut context)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| match k {
            PropertyKey::String(s) => Some(s.to_std_string_escaped()),
            _ => None,
        })
        .collect();

    let bare_result = context
        .eval(Source::from_bytes(script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute script: {e}"))?;

    let respond_fn = global
        .get(js_string!(entrypoints::RESPOND), &mut context)
        .ok()
        .filter(JsValue::is_callable);
    let result = if let Some(func) = respond_fn {
        func.as_callable()
            .ok_or_else(|| anyhow!("respond is not a function"))?
            .call(&JsValue::undefined(), &[ctx_obj], &mut context)
            .map_err(|e| anyhow!("Failed to call respond(ctx): {e}"))?
    } else {
        // B1 (issue #357, "nothing fails silently"): if the script DECLARED a function global
        // that isn't `respond` and produced no bare-expression value (undefined/null), it almost
        // certainly has a MISNAMED entrypoint (e.g. `function respnod(ctx)`). Falling through to
        // `bare_result` (→ `None`) would silently serve a normal response with no sign the script
        // never ran. Surface it as an explicit error instead. A genuine bare expression is still
        // fine: it either declared no new functions, or produced a non-undefined value.
        if bare_result.is_undefined() || bare_result.is_null() {
            let keys = global.own_property_keys(&mut context).unwrap_or_default();
            let mut declared_a_function = false;
            for key in keys {
                let name = match &key {
                    PropertyKey::String(s) => s.to_std_string_escaped(),
                    _ => continue,
                };
                if pre_existing.contains(&name) {
                    continue;
                }
                if global
                    .get(key, &mut context)
                    .ok()
                    .filter(JsValue::is_callable)
                    .is_some()
                {
                    declared_a_function = true;
                    break;
                }
            }
            if declared_a_function {
                return Err(anyhow!(
                    "script defines function(s) but none is the `respond` entrypoint \
                     (and there is no bare expression to evaluate); did you mean `respond`?"
                ));
            }
        }
        bare_result
    };

    js_value_to_fault_decision(&mut context, result, rule_id)
}

// Native function implementations that use thread-local storage

/// Map a `ctx.state` op outcome to a JS value: this ALWAYS raises a native error on a backend
/// failure (fail-loud is the whole point of the v2 `ctx.state` API, issue #358).
fn js_flow_outcome_strict<T: Into<JsValue>>(
    outcome: Option<std::result::Result<T, String>>,
) -> JsResult<JsValue> {
    match outcome {
        Some(Ok(v)) => Ok(v.into()),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// Register a native function method on a JS object.
fn register_method(
    obj: &JsObject,
    name: &str,
    func: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
    context: &mut Context,
) -> Result<()> {
    obj.set(
        PropertyKey::from(js_string!(name)),
        NativeFunction::from_fn_ptr(func).to_js_function(context.realm()),
        false,
        context,
    )
    .map(|_| ())
    .map_err(|e| anyhow!("Failed to set {name} method: {e}"))
}

// =============================================================================
// v2 `ctx` API (issue #357 Items 1-4)
// =============================================================================

fn clamp_u16_f64(n: f64) -> u16 {
    n.max(0.0).min(f64::from(u16::MAX)) as u16
}

fn parse_json_or_null(context: &mut Context, raw: &str) -> Result<JsValue> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => json_to_js(context, &v),
        Err(_) => Ok(JsValue::null()),
    }
}

/// Captures for the `ctx.request`/`ctx.response` case-insensitive `header(name)` getter: the
/// (non-lowercased) source headers, looked up case-insensitively at call time.
type HeaderGetterCaptures = std::collections::HashMap<String, String>;

fn header_getter(
    _this: &JsValue,
    args: &[JsValue],
    captures: &HeaderGetterCaptures,
    _context: &mut Context,
) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped());
    let found = name.and_then(|n| {
        captures
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&n))
            .map(|(_, v)| v.clone())
    });
    Ok(match found {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn set_header_getter(
    obj: &JsObject,
    headers: &std::collections::HashMap<String, String>,
    context: &mut Context,
) -> Result<()> {
    let native_fn = NativeFunction::from_copy_closure_with_captures(header_getter, headers.clone())
        .to_js_function(context.realm());
    obj.set(js_string!("header"), native_fn, false, context)
        .map(|_| ())
        .map_err(|e| anyhow!("Failed to set header(): {e}"))
}

fn create_request_ctx_object(context: &mut Context, request: &ScriptRequest) -> Result<JsValue> {
    let obj = create_js_object(context);
    obj.set(
        js_string!("method"),
        JsValue::from(js_string!(request.method.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.method: {e}"))?;
    obj.set(
        js_string!("path"),
        JsValue::from(js_string!(request.path.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.path: {e}"))?;

    let path_params_obj = create_js_object(context);
    for (k, v) in &request.path_params {
        path_params_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set ctx.request.pathParams.{k}: {e}"))?;
    }
    obj.set(js_string!("pathParams"), path_params_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.pathParams: {e}"))?;

    let query_obj = create_js_object(context);
    for (k, v) in &request.query {
        query_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set ctx.request.query.{k}: {e}"))?;
    }
    obj.set(js_string!("query"), query_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.query: {e}"))?;

    let headers_obj = create_js_object(context);
    for (k, v) in &request.headers {
        headers_obj
            .set(
                js_string!(k.to_ascii_lowercase()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set ctx.request.headers.{k}: {e}"))?;
    }
    obj.set(js_string!("headers"), headers_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.headers: {e}"))?;
    set_header_getter(&obj, &request.headers, context)?;

    // ctx.request.body is always the raw string (issue #357 Item 1); fall back to
    // re-serializing the parsed `body` for callers that only populated that field.
    let raw = request.raw_body.clone().unwrap_or_else(|| {
        if request.body.is_null() {
            String::new()
        } else {
            serde_json::to_string(&request.body).unwrap_or_default()
        }
    });
    let json_val = parse_json_or_null(context, &raw)?;
    obj.set(js_string!("json"), json_val, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.json: {e}"))?;
    obj.set(
        js_string!("body"),
        JsValue::from(js_string!(raw)),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.body: {e}"))?;

    Ok(obj.into())
}

fn create_response_ctx_object(
    context: &mut Context,
    response: &ScriptResponseContext,
) -> Result<JsValue> {
    let obj = create_js_object(context);
    obj.set(
        js_string!("status"),
        JsValue::from(f64::from(response.status)),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.response.status: {e}"))?;

    let headers_obj = create_js_object(context);
    for (k, v) in &response.headers {
        headers_obj
            .set(
                js_string!(k.to_ascii_lowercase()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set ctx.response.headers.{k}: {e}"))?;
    }
    obj.set(js_string!("headers"), headers_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.response.headers: {e}"))?;
    set_header_getter(&obj, &response.headers, context)?;

    let json_val = parse_json_or_null(context, &response.body)?;
    obj.set(js_string!("json"), json_val, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.response.json: {e}"))?;
    obj.set(
        js_string!("body"),
        JsValue::from(js_string!(response.body.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.response.body: {e}"))?;

    Ok(obj.into())
}

fn optional_string_to_js(value: &Option<String>) -> JsValue {
    match value {
        Some(s) => JsValue::from(js_string!(s.clone())),
        None => JsValue::null(),
    }
}

fn create_stub_ctx_object(context: &mut Context, stub: &ScriptStubContext) -> Result<JsValue> {
    let obj = create_js_object(context);
    obj.set(
        js_string!("scenarioName"),
        optional_string_to_js(&stub.scenario_name),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.scenarioName: {e}"))?;
    obj.set(
        js_string!("scenarioState"),
        optional_string_to_js(&stub.scenario_state),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.scenarioState: {e}"))?;
    obj.set(
        js_string!("id"),
        optional_string_to_js(&stub.stub_id),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.id: {e}"))?;
    Ok(obj.into())
}

/// Captures for the `ctx.state` methods: the flow id they're bound to.
type StateCaptures = String;

fn state_get(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    // `ctx.state` is ALWAYS fail-loud (issue #358): a backend failure always raises.
    let outcome = with_current_flow_store(|store| flow_result("get", store.get(flow_id, &key)));
    match outcome {
        Some(Ok(Some(value))) => json_to_js_result(ctx, &value),
        Some(Ok(None)) => Ok(JsValue::null()),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

fn state_set(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let value = args.get(1).cloned().unwrap_or(JsValue::null());
    let json_value = js_to_json(ctx, &value)?;
    let outcome = with_current_flow_store(|store| {
        flow_result("set", store.set(flow_id, &key, json_value).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

fn state_incr(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome =
        with_current_flow_store(|store| flow_result("increment", store.increment(flow_id, &key)));
    js_flow_outcome_strict(outcome)
}

fn state_exists(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome =
        with_current_flow_store(|store| flow_result("exists", store.exists(flow_id, &key)));
    js_flow_outcome_strict(outcome)
}

fn state_delete(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome = with_current_flow_store(|store| {
        flow_result("delete", store.delete(flow_id, &key).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

/// Get a value, or `default` if the key is absent (issue #358) — kills the
/// `state.x = state.x || 0` idiom. A store failure ALWAYS raises (fail-loud), never conflated with
/// "absent" the way `state_get`'s lenient fallback would.
fn state_get_or(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let default = args.get(1).cloned().unwrap_or(JsValue::null());
    let outcome = with_current_flow_store(|store| flow_result("getOr", store.get(flow_id, &key)));
    match outcome {
        Some(Ok(Some(value))) => json_to_js_result(ctx, &value),
        Some(Ok(None)) => Ok(default),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// Atomic increment by `n`, starting at 0 when absent (issue #358). Always fail-loud.
fn state_incr_by(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let by = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n as i64)
        .ok_or_else(|| JsNativeError::typ().with_message("n must be a number"))?;
    let outcome = with_current_flow_store(|store| {
        flow_result("incrementBy", store.increment_by(flow_id, &key, by))
    });
    js_flow_outcome_strict(outcome)
}

/// Convert a [`CasOutcome`] to the JS return shape for `ctx.state.cas()` (issue #358): an object —
/// `{ applied: true, current: null }` on success, or `{ applied: false, current: <value> }` on
/// conflict — deliberately an object rather than a bare value so "conflict, current value happens
/// to be `true`" can never be confused with "applied".
fn cas_outcome_to_js(context: &mut Context, outcome: CasOutcome) -> JsResult<JsValue> {
    let obj = create_js_object(context);
    let (applied, current_js) = match outcome {
        CasOutcome::Applied => (true, JsValue::null()),
        CasOutcome::Conflict(current) => {
            let current_js = match &current {
                Some(v) => json_to_js_result(context, v)?,
                None => JsValue::null(),
            };
            (false, current_js)
        }
    };
    obj.set(
        js_string!("applied"),
        JsValue::from(applied),
        false,
        context,
    )
    .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    obj.set(js_string!("current"), current_js, false, context)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    Ok(obj.into())
}

/// Atomic compare-and-set (issue #358, #311): `key` is set to `new` iff its current value equals
/// `expected` (`null`/`undefined` means "not present"). Always fail-loud.
fn state_cas(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let expected_js = args.get(1).cloned().unwrap_or(JsValue::null());
    let expected_json = js_to_json(ctx, &expected_js)?;
    let expected = if expected_json.is_null() {
        None
    } else {
        Some(expected_json)
    };
    let new_js = args.get(2).cloned().unwrap_or(JsValue::null());
    let new_json = js_to_json(ctx, &new_js)?;

    let outcome = with_current_flow_store(|store| {
        flow_result(
            "cas",
            store.compare_and_set(flow_id, &key, expected.as_ref(), new_json),
        )
    });
    match outcome {
        Some(Ok(cas_outcome)) => cas_outcome_to_js(ctx, cas_outcome),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// Per-flow TTL override in seconds (issue #358). Always fail-loud.
fn state_ttl(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let ttl_seconds = args
        .first()
        .and_then(|v| v.as_number())
        .map(|n| n as i64)
        .ok_or_else(|| JsNativeError::typ().with_message("seconds must be a number"))?;
    let outcome = with_current_flow_store(|store| {
        flow_result("ttl", store.set_ttl(flow_id, ttl_seconds).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

/// `ctx.state` — a flow-state handle bound to one flow id (issue #357 Item 1; atomic ops
/// `getOr`/`incrBy`/`cas`/`ttl` added by issue #358).
type StateMethodFn = fn(&JsValue, &[JsValue], &StateCaptures, &mut Context) -> JsResult<JsValue>;

fn create_state_object(context: &mut Context, flow_id: String) -> Result<JsValue> {
    let obj = create_js_object(context);
    let methods: [(&str, StateMethodFn); 9] = [
        ("get", state_get),
        ("set", state_set),
        ("incr", state_incr),
        ("exists", state_exists),
        ("delete", state_delete),
        ("getOr", state_get_or),
        ("incrBy", state_incr_by),
        ("cas", state_cas),
        ("ttl", state_ttl),
    ];
    for (name, func) in methods {
        let native_fn = NativeFunction::from_copy_closure_with_captures(func, flow_id.clone())
            .to_js_function(context.realm());
        obj.set(js_string!(name), native_fn, false, context)
            .map_err(|e| anyhow!("Failed to set ctx.state.{name}: {e}"))?;
    }
    Ok(obj.into())
}

fn store_flow_method(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    create_state_object(ctx, flow_id)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

/// `ctx.store`: the flow-store escape hatch (issue #357 Item 1) — `.flow(id)` returns a handle
/// scoped to an arbitrary flow id.
fn create_store_object(context: &mut Context) -> Result<JsValue> {
    let obj = create_js_object(context);
    register_method(&obj, "flow", store_flow_method, context)?;
    Ok(obj.into())
}

/// Build the v2 `ctx` object (issue #357 Item 1): identical field names/semantics across engines —
/// see the doc comment on [`ScriptCtxInput`]. `ctx.logger` reuses P1's native logger (issue #355).
fn create_ctx_object(context: &mut Context, input: &ScriptCtxInput) -> Result<JsValue> {
    let obj = create_js_object(context);

    let request_obj = create_request_ctx_object(context, input.request)?;
    obj.set(js_string!("request"), request_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request: {e}"))?;

    if let Some(resp) = &input.response {
        let response_obj = create_response_ctx_object(context, resp)?;
        obj.set(js_string!("response"), response_obj, false, context)
            .map_err(|e| anyhow!("Failed to set ctx.response: {e}"))?;
    }

    obj.set(
        js_string!("flowId"),
        JsValue::from(js_string!(input.flow_id.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.flowId: {e}"))?;

    let stub_obj = create_stub_ctx_object(context, &input.stub)?;
    obj.set(js_string!("stub"), stub_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.stub: {e}"))?;

    let state_obj = create_state_object(context, input.flow_id.clone())?;
    obj.set(js_string!("state"), state_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.state: {e}"))?;

    let store_obj = create_store_object(context)?;
    obj.set(js_string!("store"), store_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.store: {e}"))?;

    let logger_obj = create_script_logger_object(context, input.port, input.stub.stub_id.clone())?;
    obj.set(js_string!("logger"), logger_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.logger: {e}"))?;

    Ok(obj.into())
}

// =============================================================================
// v2 result constructors: http()/delay()/reset()/pass() (issue #357 Item 3)
// =============================================================================

fn wrap_script_result(context: &mut Context, result: ScriptResult) -> JsResult<JsValue> {
    let id = SCRIPT_RESULT_NEXT_ID.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    SCRIPT_RESULT_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, result);
    });
    let obj = create_js_object(context);
    obj.set(
        js_string!("__riftResultId"),
        JsValue::from(id as f64),
        false,
        context,
    )?;
    obj.set(
        PropertyKey::from(js_string!("header")),
        NativeFunction::from_fn_ptr(script_result_header).to_js_function(context.realm()),
        false,
        context,
    )?;
    Ok(obj.into())
}

fn script_result_header(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = this
        .as_object()
        .and_then(|o| o.get(js_string!("__riftResultId"), ctx).ok())
        .and_then(|v| v.as_number())
        .map(|n| n as u64)
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header() called on a non-result value")
        })?;
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header(name, value): name must be a string")
        })?;
    let value = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header(name, value): value must be a string")
        })?;
    SCRIPT_RESULT_REGISTRY.with(|r| {
        if let Some(res) = r.borrow_mut().get_mut(&id) {
            res.add_header(key, value);
        }
    });
    Ok(this.clone())
}

fn js_value_to_script_result_body(ctx: &mut Context, v: &JsValue) -> JsResult<ScriptResultBody> {
    if let Some(s) = v.as_string() {
        Ok(ScriptResultBody::Str(s.to_std_string_escaped()))
    } else {
        Ok(ScriptResultBody::Json(js_to_json(ctx, v)?))
    }
}

fn ctor_http(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let status = args.first().and_then(JsValue::as_number).unwrap_or(200.0);
    let body = match args.get(1) {
        None => None,
        Some(v) if v.is_null() || v.is_undefined() => None,
        Some(v) => Some(js_value_to_script_result_body(ctx, v)?),
    };
    wrap_script_result(ctx, ScriptResult::http(clamp_u16_f64(status), body))
}

fn ctor_delay(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = args.first().and_then(JsValue::as_number).unwrap_or(0.0);
    wrap_script_result(ctx, ScriptResult::Delay(ms.max(0.0) as u64))
}

fn ctor_reset(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    wrap_script_result(ctx, ScriptResult::Reset)
}

fn ctor_pass(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    wrap_script_result(ctx, ScriptResult::Pass)
}

/// Register the v2 result constructors as globals. Cheap and idempotent, so it's called on every
/// script run rather than requiring every `Context` creation site in this module to remember it.
fn register_result_constructors(context: &mut Context) -> Result<()> {
    let global = context.global_object();
    for (name, func) in [
        (
            "http",
            ctor_http as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
        ),
        ("delay", ctor_delay),
        ("reset", ctor_reset),
        ("pass", ctor_pass),
    ] {
        global
            .set(
                js_string!(name),
                NativeFunction::from_fn_ptr(func).to_js_function(context.realm()),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set global {name}: {e}"))?;
    }
    Ok(())
}

/// Convert a `respond(ctx)`/bare-expression return value into a [`FaultDecision`] (issue #357
/// Item 3): `null`/`undefined` (or the script returning nothing) → `None`; an `http()`/`delay()`/
/// `reset()`/`pass()` result → its own outcome; anything else is an error.
fn js_value_to_fault_decision(
    context: &mut Context,
    value: JsValue,
    rule_id: &str,
) -> Result<FaultDecision> {
    if value.is_null() || value.is_undefined() {
        return Ok(FaultDecision::None);
    }
    let id = value
        .as_object()
        .and_then(|o| o.get(js_string!("__riftResultId"), context).ok())
        .and_then(|v| v.as_number())
        .map(|n| n as u64);
    let Some(id) = id else {
        return Err(anyhow!(
            "respond(ctx) must return http(...)/delay(...)/reset()/pass() or nothing"
        ));
    };
    let result = SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().remove(&id));
    let result = result.ok_or_else(|| anyhow!("respond(ctx) result was already consumed"))?;
    Ok(result.into_fault_decision(rule_id))
}

// =============================================================================
// Mountebank v2 `config` calling convention support (issue #355 Item 0)
// =============================================================================

/// Flatten every field of a Mountebank request object onto `config` itself
/// (`config.method`, `config.path`, `config.query`, `config.headers`, `config.body`), mirroring
/// Mountebank v2's `downcastInjectionConfig`. This is why `function(config){config.request.path}`,
/// `function(request){request.path}` (with `config` passed positionally where `request` used to
/// be), and `function(config){config.path}` all address the same data.
fn flatten_request_onto(
    config_obj: &JsObject,
    request_obj: &JsValue,
    context: &mut Context,
) -> Result<()> {
    let Some(req) = request_obj.as_object() else {
        return Ok(());
    };
    for key in ["method", "path", "query", "headers", "body"] {
        let val = req
            .get(js_string!(key), context)
            .map_err(|e| anyhow!("Failed to read request.{key} for config flattening: {e}"))?;
        config_obj
            .set(js_string!(key), val, false, context)
            .map_err(|e| anyhow!("Failed to flatten config.{key}: {e}"))?;
    }
    Ok(())
}

// =============================================================================
// Native logger (issue #355 Item 1)
// =============================================================================

/// Log one Mountebank script `logger.<level>(...)` call at target `"rift::script"`, tagging the
/// event with the imposter port and (where available) the stub id. All arguments are best-effort
/// stringified and joined with a space, mirroring a console-style logger call.
fn log_script_event(
    level: tracing::Level,
    port: u16,
    stub_id: Option<&str>,
    args: &[JsValue],
    context: &mut Context,
) {
    let message: String = args
        .iter()
        .map(|v| js_value_to_log_string(context, v))
        .collect::<Vec<_>>()
        .join(" ");
    let stub_id = stub_id.unwrap_or("");
    match level {
        tracing::Level::ERROR => {
            tracing::error!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::WARN => {
            tracing::warn!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::INFO => {
            tracing::info!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::DEBUG => {
            tracing::debug!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::TRACE => {
            tracing::trace!(target: "rift::script", port, stub_id, "{message}")
        }
    }
}

/// Best-effort stringify of a single logger argument: strings pass through; objects are
/// JSON-stringified; everything else uses JS `display()`.
fn js_value_to_log_string(context: &mut Context, value: &JsValue) -> String {
    if let Some(s) = value.as_string() {
        return s.to_std_string_escaped();
    }
    if value.is_object() {
        let json = js_to_json(context, value).unwrap_or(Value::Null);
        return serde_json::to_string(&json).unwrap_or_default();
    }
    value.display().to_string()
}

/// Captures shared by every native logger method registered on one script's `logger` object:
/// the imposter port and (where the caller has one) the stub id.
type LoggerCaptures = (u16, Option<String>);

fn logger_debug(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::DEBUG,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_info(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::INFO,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_warn(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::WARN,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_error(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::ERROR,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

/// Build a native `logger` object (`debug`/`info`/`warn`/`error`) that routes to `tracing` at
/// target `"rift::script"`, tagged with the imposter port and, where available, the stub id
/// (issue #355 Item 1). A real native object — not a JS no-op shim — so script log calls actually
/// reach the process's tracing subscriber.
/// Function pointer type shared by every native logger method (see [`create_script_logger_object`]).
type LoggerMethodFn = fn(&JsValue, &[JsValue], &LoggerCaptures, &mut Context) -> JsResult<JsValue>;

fn create_script_logger_object(
    context: &mut Context,
    port: u16,
    stub_id: Option<String>,
) -> Result<JsValue> {
    let obj = create_js_object(context);
    let captures: LoggerCaptures = (port, stub_id);

    let methods: [(&str, LoggerMethodFn); 4] = [
        ("debug", logger_debug),
        ("info", logger_info),
        ("warn", logger_warn),
        ("error", logger_error),
    ];

    for (name, func) in methods {
        let native_fn = NativeFunction::from_copy_closure_with_captures(func, captures.clone())
            .to_js_function(context.realm());
        obj.set(js_string!(name), native_fn, false, context)
            .map_err(|e| anyhow!("Failed to set logger.{name}: {e}"))?;
    }

    Ok(obj.into())
}

/// Convert JSON Value to JavaScript value
fn json_to_js(context: &mut Context, value: &Value) -> Result<JsValue> {
    json_to_js_result(context, value).map_err(|e| anyhow!("Failed to convert JSON to JS: {e}"))
}

/// Convert JSON Value to JavaScript value (JsResult version)
fn json_to_js_result(context: &mut Context, value: &Value) -> JsResult<JsValue> {
    match value {
        Value::Null => Ok(JsValue::null()),
        Value::Bool(b) => Ok(JsValue::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(JsValue::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(JsValue::from(f))
            } else {
                Ok(JsValue::null())
            }
        }
        Value::String(s) => Ok(JsValue::from(js_string!(s.clone()))),
        Value::Array(arr) => {
            let js_arr = JsArray::new(context);
            for (i, v) in arr.iter().enumerate() {
                let js_val = json_to_js_result(context, v)?;
                js_arr.set(i as u32, js_val, false, context)?;
            }
            Ok(js_arr.into())
        }
        Value::Object(obj) => {
            let js_obj = create_js_object(context);
            for (k, v) in obj {
                let js_val = json_to_js_result(context, v)?;
                js_obj.set(js_string!(k.clone()), js_val, false, context)?;
            }
            Ok(js_obj.into())
        }
    }
}

/// Convert JavaScript value to JSON Value
fn js_to_json(context: &mut Context, value: &JsValue) -> JsResult<Value> {
    if value.is_null() || value.is_undefined() {
        return Ok(Value::Null);
    }

    if let Some(b) = value.as_boolean() {
        return Ok(Value::Bool(b));
    }

    if let Some(n) = value.as_number() {
        // JS has a single number type, so a whole number like `5` arrives as the f64 `5.0`.
        // Encoding it via `from_f64` would store `5.0` (JSON `is_i64() == false`), which the
        // integer-only `increment`/`increment_by` and Redis `INCRBY` can't accumulate — a later
        // `incr` would silently start from 0 (issue #358 B2). So when the value is integral and in
        // i64 range, emit an integer `Number` to keep counters usable across set→incr.
        let number = if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            serde_json::Number::from(n as i64)
        } else {
            serde_json::Number::from_f64(n).unwrap_or(serde_json::Number::from(0))
        };
        return Ok(Value::Number(number));
    }

    if let Some(s) = value.as_string() {
        return Ok(Value::String(s.to_std_string_escaped()));
    }

    if let Some(obj) = value.as_object() {
        // Check if it's an array
        if obj.is_array() {
            let len = obj
                .get(js_string!("length"), context)?
                .as_number()
                .unwrap_or(0.0) as u32;
            let mut arr = Vec::new();
            for i in 0..len {
                let item = obj.get(i, context)?;
                arr.push(js_to_json(context, &item)?);
            }
            return Ok(Value::Array(arr));
        }

        // It's a regular object
        let mut map = serde_json::Map::new();
        if let Ok(keys) = obj.own_property_keys(context) {
            for key in keys {
                let key_str = match &key {
                    PropertyKey::String(s) => s.to_std_string_escaped(),
                    PropertyKey::Index(i) => i.get().to_string(),
                    PropertyKey::Symbol(_) => continue, // Skip symbols
                };
                if let Ok(val) = obj.get(key.clone(), context) {
                    map.insert(key_str, js_to_json(context, &val)?);
                }
            }
        }
        return Ok(Value::Object(map));
    }

    Ok(Value::Null)
}

// =============================================================================
// Mountebank-style inject response support
// =============================================================================

/// Response from a Mountebank inject function
#[derive(Debug, Clone)]
pub struct MountebankInjectResponse {
    pub status_code: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body: String,
}

/// Per-imposter state storage for inject functions
/// This is used to share state between inject function calls
use std::sync::{LazyLock, Mutex};

static IMPOSTER_STATE: LazyLock<
    Mutex<std::collections::HashMap<u16, serde_json::Map<String, Value>>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Get or create state for an imposter
fn get_imposter_state(port: u16) -> serde_json::Map<String, Value> {
    let states = IMPOSTER_STATE.lock().unwrap();
    states.get(&port).cloned().unwrap_or_default()
}

/// Save state for an imposter
fn save_imposter_state(port: u16, state: serde_json::Map<String, Value>) {
    let mut states = IMPOSTER_STATE.lock().unwrap();
    states.insert(port, state);
}

/// Clear state for an imposter (called when imposter is deleted)
pub fn clear_imposter_state(port: u16) {
    let mut states = IMPOSTER_STATE.lock().unwrap();
    states.remove(&port);
}

/// Execute a Mountebank-style inject function.
///
/// Implements Mountebank v2's `config`-first calling convention (issue #355 Item 0), built like
/// Mountebank's `downcastInjectionConfig`: a `config` object (`{ request, state, logger,
/// callback }`) with every request field ALSO flattened onto `config` itself, so
/// `function(config){config.request.path}`, `function(request){request.path}` (legacy — `config`
/// is passed where `request` used to be, and works because of the flattening), and
/// `function(config){config.path}` all resolve the same data. Legacy positional args follow
/// `config` unchanged, so old scripts keep working:
///
/// - `function(config) { return response; }` (v2)
/// - `function(request) { return response; }` (legacy, `config` stands in for `request`)
/// - `function(request, state) { return response; }`
/// - `function(request, state, logger, callback) { callback(response); }`
///
/// Full call signature: `fn(config, injectState, logger, done, imposterState)`. `injectState`
/// (arg 2) and `imposterState` (arg 5) are the SAME object as `config.state` — the per-imposter
/// state persisted across calls via `get_imposter_state`/`save_imposter_state`, shared with
/// predicate injects and decorate for the same imposter port. `done` (arg 4, aliased as
/// `config.callback`) is the async-style callback; if the function returns `undefined` its
/// invocation is used instead (this engine is fully synchronous, so "waiting" for `done` just
/// means checking whether it was called during the synchronous call).
///
/// Where response is: `{ statusCode: number, headers: object?, body: string }`
pub fn execute_mountebank_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    imposter_port: u16,
    stub_id: Option<&str>,
) -> Result<MountebankInjectResponse> {
    let mut context = bounded_js_context();

    // Create request object
    let request_obj = create_mountebank_request_object(&mut context, request)?;

    // Get current (persisted, per-port) state for this imposter — shared across predicate
    // injects, response injects, and decorate for the same imposter (issue #355 Item 0).
    let state_map = get_imposter_state(imposter_port);
    let state_obj = json_to_js(&mut context, &Value::Object(state_map))?;

    let logger_obj =
        create_script_logger_object(&mut context, imposter_port, stub_id.map(str::to_owned))?;

    // Build config = { request, state, logger } and flatten request fields onto config itself.
    let config_obj = create_js_object(&context);
    config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.request: {e}"))?;
    config_obj
        .set(js_string!("state"), state_obj.clone(), false, &mut context)
        .map_err(|e| anyhow!("Failed to set config.state: {e}"))?;
    config_obj
        .set(
            js_string!("logger"),
            logger_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.logger: {e}"))?;
    flatten_request_onto(&config_obj, &request_obj, &mut context)?;

    // Set global variables consumed by the wrapper script below.
    let global = context.global_object();
    global
        .set(js_string!("__config"), config_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set config: {e}"))?;
    global
        .set(js_string!("__logger"), logger_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set logger: {e}"))?;
    // injectState and imposterState are the SAME shared, persisted state object as config.state.
    global
        .set(
            js_string!("__injectState"),
            state_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set injectState: {e}"))?;
    global
        .set(
            js_string!("__imposterState"),
            state_obj,
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set imposterState: {e}"))?;

    // Wrap the inject function to call it with (config, injectState, logger, done, imposterState).
    // `config.callback` aliases `done` so scripts using either the positional callback or the
    // config field convention both work. If the function returns undefined, use whatever `done`/
    // `config.callback` was invoked with instead (synchronous stand-in for MB's async wait).
    let wrapper_script = format!(
        r#"
        var __callbackResult = null;
        var __done = function(r) {{ __callbackResult = r; }};
        __config.callback = __done;
        var __injectFn = {inject_fn};
        var __directResult = __injectFn(__config, __injectState, __logger, __done, __imposterState);
        if (__directResult === undefined) {{
            __directResult = __callbackResult;
        }}
        __directResult;
        "#
    );

    // Execute the script
    let result = context
        .eval(Source::from_bytes(wrapper_script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute inject function: {e}"))?;

    // Save updated state back (config.state/injectState/imposterState all alias the same object).
    let updated_state = global
        .get(js_string!("__imposterState"), &mut context)
        .map_err(|e| anyhow!("Failed to get updated state: {e}"))?;

    if let Ok(Value::Object(map)) = js_to_json(&mut context, &updated_state) {
        save_imposter_state(imposter_port, map);
    }

    // Parse the response
    parse_mountebank_response(&mut context, result)
}

/// Execute a Mountebank-style predicate inject function.
///
/// v2 `config`-first calling convention (issue #355 Item 0): full signature
/// `fn(config, logger, imposterState) { return bool; }`, where `config` also stands in for the
/// legacy `request` positional argument (request fields are flattened onto `config`), and
/// `imposterState` aliases `config.state` — the same per-port state shared with response injects
/// and decorate for the same imposter. Returns `true` if the predicate matches, `false` otherwise.
pub fn execute_predicate_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    imposter_port: u16,
) -> bool {
    let mut context = bounded_js_context();

    let request_obj = match create_mountebank_request_object(&mut context, request) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("inject predicate: failed to build request object: {e}");
            return false;
        }
    };

    let state_map = get_imposter_state(imposter_port);
    let state_obj = match json_to_js(&mut context, &Value::Object(state_map)) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("inject predicate: failed to build state object: {e}");
            return false;
        }
    };

    // stub_id is left None here: predicate matching operates on a `PredicateOperation` tree and
    // the owning stub's id is not threaded through the matcher without an invasive change to the
    // predicate-matching signatures (issue #355 AC1 note).
    let logger_obj = match create_script_logger_object(&mut context, imposter_port, None) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("inject predicate: failed to build logger object: {e}");
            return false;
        }
    };

    let config_obj = create_js_object(&context);
    if config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .is_err()
        || config_obj
            .set(js_string!("state"), state_obj.clone(), false, &mut context)
            .is_err()
        || config_obj
            .set(
                js_string!("logger"),
                logger_obj.clone(),
                false,
                &mut context,
            )
            .is_err()
        || flatten_request_onto(&config_obj, &request_obj, &mut context).is_err()
    {
        tracing::warn!("inject predicate: failed to build config object");
        return false;
    }

    let global = context.global_object();
    let _ = global.set(js_string!("__config"), config_obj, false, &mut context);
    let _ = global.set(js_string!("__logger"), logger_obj, false, &mut context);
    let _ = global.set(
        js_string!("__imposterState"),
        state_obj,
        false,
        &mut context,
    );

    let wrapper_script = format!(
        r#"
        var __injectFn = {inject_fn};
        var __result = __injectFn(__config, __logger, __imposterState);
        Boolean(__result);
        "#
    );

    let result = match context.eval(Source::from_bytes(wrapper_script.as_bytes())) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("inject predicate: script execution error: {e}");
            return false;
        }
    };

    // Update the persisted, shared per-port state.
    if let Ok(updated_state) = global.get(js_string!("__imposterState"), &mut context)
        && let Ok(Value::Object(map)) = js_to_json(&mut context, &updated_state)
    {
        save_imposter_state(imposter_port, map);
    }

    result.to_boolean()
}

/// Execute a `predicateGenerators.inject` function during proxy recording.
///
/// The function receives `(config, logger, predicates)` where `config.request` is the
/// proxied request and `predicates` is the array built so far by `matches`-based generators.
/// It returns an array of predicate objects that are appended to the final predicate list.
pub fn execute_predicate_generator_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    existing_predicates: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut context = bounded_js_context();

    let request_obj = match create_mountebank_request_object(&mut context, request) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("predicateGenerator inject: failed to build request object: {e}");
            return Vec::new();
        }
    };

    let predicates_val = match json_to_js(&mut context, &Value::Array(existing_predicates.to_vec()))
    {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("predicateGenerator inject: failed to build predicates array: {e}");
            return Vec::new();
        }
    };

    // No imposter is running yet during proxy recording, so there is no per-port state to tag
    // the logger with (port 0 placeholder) and no stub id exists yet (stub_id left None).
    let logger_obj = match create_script_logger_object(&mut context, 0, None) {
        Ok(obj) => obj,
        Err(e) => {
            tracing::warn!("predicateGenerator inject: failed to build logger object: {e}");
            return Vec::new();
        }
    };

    let global = context.global_object();
    let _ = global.set(js_string!("__request"), request_obj, false, &mut context);
    let _ = global.set(js_string!("__logger"), logger_obj, false, &mut context);
    let _ = global.set(
        js_string!("__predicates"),
        predicates_val,
        false,
        &mut context,
    );

    let wrapper_script = format!(
        r#"
        var __injectFn = {inject_fn};
        var __config = {{ request: __request }};
        var __result = __injectFn(__config, __logger, __predicates);
        JSON.stringify(__result);
        "#
    );

    let result = match context.eval(Source::from_bytes(wrapper_script.as_bytes())) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("predicateGenerator inject: script execution error: {e}");
            return Vec::new();
        }
    };

    let json_str = match result.as_string() {
        Some(s) => s.to_std_string_lossy(),
        None => {
            tracing::warn!(
                "predicateGenerator inject: function did not return a stringifiable value"
            );
            return Vec::new();
        }
    };

    match serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
        Ok(preds) => preds,
        Err(e) => {
            tracing::warn!("predicateGenerator inject: failed to parse returned predicates: {e}");
            Vec::new()
        }
    }
}

/// Request structure for Mountebank inject functions
#[derive(Debug, Clone)]
pub struct MountebankRequest {
    pub method: String,
    pub path: String,
    pub query: std::collections::HashMap<String, String>,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<String>,
}

/// Create a Mountebank-style request object
fn create_mountebank_request_object(
    context: &mut Context,
    request: &MountebankRequest,
) -> Result<JsValue> {
    let obj = create_js_object(context);

    // Set method
    obj.set(
        js_string!("method"),
        JsValue::from(js_string!(request.method.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set method: {e}"))?;

    // Set path
    obj.set(
        js_string!("path"),
        JsValue::from(js_string!(request.path.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set path: {e}"))?;

    // Set query
    let query_obj = create_js_object(context);
    for (k, v) in &request.query {
        query_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set query param: {e}"))?;
    }
    obj.set(js_string!("query"), query_obj, false, context)
        .map_err(|e| anyhow!("Failed to set query: {e}"))?;

    // Set headers
    let headers_obj = create_js_object(context);
    for (k, v) in &request.headers {
        headers_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    obj.set(js_string!("headers"), headers_obj, false, context)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // Set body - always as a string to match Mountebank behavior
    // Users should call JSON.parse(request.body) themselves if they want parsed JSON
    if let Some(body) = &request.body {
        obj.set(
            js_string!("body"),
            JsValue::from(js_string!(body.clone())),
            false,
            context,
        )
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;
    } else {
        obj.set(js_string!("body"), JsValue::undefined(), false, context)
            .map_err(|e| anyhow!("Failed to set body: {e}"))?;
    }

    Ok(obj.into())
}

/// Parse Mountebank inject response
fn parse_mountebank_response(
    context: &mut Context,
    result: JsValue,
) -> Result<MountebankInjectResponse> {
    let obj = result
        .as_object()
        .ok_or_else(|| anyhow!("Inject function must return an object"))?;

    // Get statusCode (required)
    let status_code = obj
        .get(js_string!("statusCode"), context)
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u16)
        .unwrap_or(200);

    // Get headers (optional)
    let mut headers = std::collections::HashMap::new();
    if let Ok(headers_val) = obj.get(js_string!("headers"), context)
        && let Some(headers_obj) = headers_val.as_object()
        && let Ok(keys) = headers_obj.own_property_keys(context)
    {
        for key in keys {
            let key_str = match &key {
                PropertyKey::String(s) => s.to_std_string_escaped(),
                PropertyKey::Index(i) => i.get().to_string(),
                PropertyKey::Symbol(_) => continue,
            };
            if let Ok(val) = headers_obj.get(key.clone(), context)
                && let Some(s) = val.as_string()
            {
                headers.insert(key_str, s.to_std_string_escaped());
            }
        }
    }

    // Get body (optional)
    let body = obj
        .get(js_string!("body"), context)
        .ok()
        .map(|v| {
            if let Some(s) = v.as_string() {
                s.to_std_string_escaped()
            } else if v.is_object() {
                // Convert object to JSON string
                let json = js_to_json(context, &v).unwrap_or(Value::Null);
                serde_json::to_string(&json).unwrap_or_default()
            } else if v.is_null() || v.is_undefined() {
                String::new()
            } else {
                v.display().to_string()
            }
        })
        .unwrap_or_default();

    Ok(MountebankInjectResponse {
        status_code,
        headers,
        body,
    })
}

/// Response from a Mountebank decorate function
#[derive(Debug, Clone)]
pub struct MountebankDecorateResponse {
    pub status_code: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body: String,
}

/// Minimal CommonJS module loader backing the `require()` global (issue #305).
///
/// Resolves `path` (absolute as-is, relative against the process CWD), reads the source,
/// wraps it as `(function(module, exports, __filename, __dirname) { ... })` and evaluates it.
/// Nested `require(...)` calls inside the loaded module resolve through the same global
/// `require`, since the wrapper does not shadow it as a parameter.
///
/// `cache` memoizes `module.exports` by canonicalized path (falling back to the resolved path
/// string when canonicalization fails) so requiring the same module twice within one decorate
/// run only reads and evaluates it once.
type RequireCache =
    boa_engine::gc::Gc<boa_engine::gc::GcRefCell<std::collections::HashMap<String, JsValue>>>;

fn require_impl(
    cache: &RequireCache,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let path_str = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("require() path must be a string"))?;

    let raw_path = std::path::PathBuf::from(&path_str);
    let resolved_path = if raw_path.is_absolute() {
        raw_path
    } else {
        let cwd = std::env::current_dir().map_err(|e| {
            JsNativeError::error()
                .with_message(format!("require('{path_str}'): cannot resolve cwd: {e}"))
        })?;
        cwd.join(raw_path)
    };

    let cache_key = std::fs::canonicalize(&resolved_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved_path.to_string_lossy().into_owned());

    if let Some(cached) = cache.borrow().get(&cache_key) {
        return Ok(cached.clone());
    }

    let source = std::fs::read_to_string(&resolved_path)
        .map_err(|e| JsNativeError::error().with_message(format!("require('{path_str}'): {e}")))?;

    let wrapped_source =
        format!("(function(module, exports, __filename, __dirname) {{\n{source}\n}})");
    let wrapper = context.eval(Source::from_bytes(wrapped_source.as_bytes()))?;
    let wrapper_obj = wrapper.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message(format!(
            "require('{path_str}'): module wrapper is not callable"
        ))
    })?;

    let module_obj = create_js_object(context);
    let exports_obj = create_js_object(context);
    module_obj.set(
        js_string!("exports"),
        JsValue::from(exports_obj.clone()),
        false,
        context,
    )?;

    // Publish the (still-empty) exports to the cache BEFORE evaluating the module, so a circular
    // require() returns the partial module instead of recursing until the native stack overflows
    // (Node's cycle-breaking contract). The final value is re-published after eval below.
    cache
        .borrow_mut()
        .insert(cache_key.clone(), JsValue::from(exports_obj.clone()));

    let filename = resolved_path.to_string_lossy().into_owned();
    let dirname = resolved_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    wrapper_obj.call(
        &JsValue::undefined(),
        &[
            JsValue::from(module_obj.clone()),
            JsValue::from(exports_obj),
            JsValue::from(js_string!(filename)),
            JsValue::from(js_string!(dirname)),
        ],
        context,
    )?;

    // A module can reassign `module.exports` (e.g. `module.exports = function ...`); publish the
    // final value so later require()s of the same module get the real exports, not the placeholder.
    let module_exports = module_obj.get(js_string!("exports"), context)?;
    cache.borrow_mut().insert(cache_key, module_exports.clone());

    Ok(module_exports)
}

/// Register a global `require()` implementing the minimal CommonJS loader above.
///
/// The module cache is a GC-traced `Gc<GcRefCell<HashMap<..>>>` passed as the native function's
/// capture, so it is shared across every `require()` call made during one script evaluation
/// (including nested requires triggered by a loaded module) and its cached `JsValue`s stay
/// reachable to the collector for the lifetime of the enclosing `Context`.
fn register_require(context: &mut Context) -> Result<()> {
    // The cache holds `JsValue`s (module.exports), which are GC-managed, so it must be a
    // GC-traced capture — a plain `Rc<RefCell<..>>` closure capture would be untraced and could
    // let the GC free a cached export still referenced only from the cache (UB). `Gc<GcRefCell>`
    // is traced, so `from_copy_closure_with_captures` registers it safely (no `unsafe`).
    let cache: RequireCache = boa_engine::gc::Gc::new(boa_engine::gc::GcRefCell::new(
        std::collections::HashMap::new(),
    ));

    let require_fn = NativeFunction::from_copy_closure_with_captures(
        |_this, args, cache: &RequireCache, context| require_impl(cache, args, context),
        cache,
    );

    let global = context.global_object();
    global
        .set(
            js_string!("require"),
            require_fn.to_js_function(context.realm()),
            false,
            context,
        )
        .map_err(|e| anyhow!("Failed to set require: {e}"))?;

    Ok(())
}

/// Execute a Mountebank `config => {...}` / `function(config)` decorate in Boa, exposing a
/// `config` object ({ request, response, path, state, logger }) — with every request field also
/// flattened onto `config` itself (issue #355 Item 0) — and a CommonJS `require()` so a decorate
/// can load an external `.cjs`/`.js` module (issue #305). `config.state` is the per-port state
/// persisted via `get_imposter_state`/`save_imposter_state`, shared with predicate injects and
/// response injects for the same imposter. Returns the mutated response.
pub fn execute_mountebank_config_decorate(
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: u16,
    response_headers: &std::collections::HashMap<String, String>,
    imposter_port: u16,
    stub_id: Option<&str>,
) -> Result<MountebankDecorateResponse> {
    let mut context = bounded_js_context();

    // Create request object
    let request_obj = create_mountebank_request_object(&mut context, request)?;

    // Create response object
    let response_obj = create_js_object(&context);

    response_obj
        .set(
            js_string!("statusCode"),
            JsValue::from(response_status as i32),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set statusCode: {e}"))?;

    response_obj
        .set(
            js_string!("body"),
            JsValue::from(js_string!(response_body.to_string())),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;

    // Create headers object for response
    let headers_obj = create_js_object(&context);
    for (k, v) in response_headers {
        headers_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                &mut context,
            )
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    response_obj
        .set(js_string!("headers"), headers_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // Get current (persisted, per-port) state for this imposter — shared with predicate/response
    // injects for the same imposter (issue #355 Item 0).
    let state_map = get_imposter_state(imposter_port);
    let state_obj = json_to_js(&mut context, &Value::Object(state_map))?;
    let logger_obj =
        create_script_logger_object(&mut context, imposter_port, stub_id.map(str::to_owned))?;

    // Build the config object: { request, response, path, state, logger } + flattened request.
    let config_obj = create_js_object(&context);
    config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.request: {e}"))?;
    config_obj
        .set(
            js_string!("response"),
            JsValue::from(response_obj),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.response: {e}"))?;
    config_obj
        .set(
            js_string!("path"),
            JsValue::from(js_string!(request.path.clone())),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.path: {e}"))?;
    config_obj
        .set(js_string!("state"), state_obj.clone(), false, &mut context)
        .map_err(|e| anyhow!("Failed to set config.state: {e}"))?;
    config_obj
        .set(js_string!("logger"), logger_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set config.logger: {e}"))?;
    flatten_request_onto(&config_obj, &request_obj, &mut context)?;

    // Register the CommonJS `require()` global before evaluating the decorate so it (and any
    // module it loads) can use it.
    register_require(&mut context)?;

    // Set global variable
    let global = context.global_object();
    global
        .set(
            js_string!("__config"),
            config_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config: {e}"))?;

    // Wrap the decorate function to call it with our config object (already set as the
    // `__config` global above).
    let wrapper_script = format!(
        r#"
        var __configFn = {decorate_fn};
        __configFn(__config);
        __config.response;
        "#
    );

    // Execute the script
    let result = context
        .eval(Source::from_bytes(wrapper_script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute config decorate function: {e}"))?;

    // Parse the modified response
    let obj = result.as_object().ok_or_else(|| {
        anyhow!("Config decorate function must leave config.response as an object")
    })?;

    // Get statusCode
    let status_code = obj
        .get(js_string!("statusCode"), &mut context)
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u16)
        .unwrap_or(response_status);

    // Get headers
    let mut headers = response_headers.clone();
    if let Ok(headers_val) = obj.get(js_string!("headers"), &mut context)
        && let Some(headers_obj) = headers_val.as_object()
        && let Ok(keys) = headers_obj.own_property_keys(&mut context)
    {
        for key in keys {
            let key_str = match &key {
                PropertyKey::String(s) => s.to_std_string_escaped(),
                PropertyKey::Index(i) => i.get().to_string(),
                PropertyKey::Symbol(_) => continue,
            };
            if let Ok(val) = headers_obj.get(key.clone(), &mut context)
                && let Some(s) = val.as_string()
            {
                headers.insert(key_str, s.to_std_string_escaped());
            }
        }
    }

    // Get body
    let body = obj
        .get(js_string!("body"), &mut context)
        .ok()
        .map(|v| {
            if let Some(s) = v.as_string() {
                s.to_std_string_escaped()
            } else if v.is_object() {
                let json = js_to_json(&mut context, &v).unwrap_or(Value::Null);
                serde_json::to_string(&json).unwrap_or_default()
            } else if v.is_null() || v.is_undefined() {
                String::new()
            } else {
                v.display().to_string()
            }
        })
        .unwrap_or_else(|| response_body.to_string());

    // Persist any mutation the decorate made to the shared, per-port state.
    if let Ok(Value::Object(map)) = js_to_json(&mut context, &state_obj) {
        save_imposter_state(imposter_port, map);
    }

    Ok(MountebankDecorateResponse {
        status_code,
        headers,
        body,
    })
}

/// Execute a Mountebank-style decorate behavior function.
///
/// Full call signature (issue #355 Item 0): `fn(config, response, logger, state)`, where `config`
/// stands in for the legacy `request` positional argument (request fields flattened onto
/// `config`), and `state` is the per-port state persisted via `get_imposter_state`/
/// `save_imposter_state` — shared with predicate injects and response injects for the same
/// imposter (previously this was a throwaway `{}` per call).
/// Legacy forms: `function(request, response) { ... }`, `function(request, response, logger)`,
/// `function(request, response, logger, state)`.
pub fn execute_mountebank_decorate(
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: u16,
    response_headers: &std::collections::HashMap<String, String>,
    imposter_port: u16,
    stub_id: Option<&str>,
) -> Result<MountebankDecorateResponse> {
    let mut context = bounded_js_context();

    // Create request object
    let request_obj = create_mountebank_request_object(&mut context, request)?;

    // Create response object
    let response_obj = create_js_object(&context);

    response_obj
        .set(
            js_string!("statusCode"),
            JsValue::from(response_status as i32),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set statusCode: {e}"))?;

    response_obj
        .set(
            js_string!("body"),
            JsValue::from(js_string!(response_body.to_string())),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;

    // Create headers object for response
    let headers_obj = create_js_object(&context);
    for (k, v) in response_headers {
        headers_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                &mut context,
            )
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    response_obj
        .set(js_string!("headers"), headers_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // Config-first convention (issue #355 Item 0): config stands in for the legacy `request` arg,
    // with request fields flattened onto it, and carries the shared per-port state + native
    // logger. Built even though this entrypoint's legacy convention doesn't take a `config`
    // param, so `function(config, response, ...)`-style scripts also work through this path.
    let config_obj = create_js_object(&context);
    config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set config.request: {e}"))?;
    flatten_request_onto(&config_obj, &request_obj, &mut context)?;

    // Get current (persisted, per-port) state for this imposter — shared with predicate/response
    // injects for the same imposter (previously a throwaway `{}` per call; issue #355 Item 0).
    let state_map = get_imposter_state(imposter_port);
    let state_obj = json_to_js(&mut context, &Value::Object(state_map))?;
    let logger_obj =
        create_script_logger_object(&mut context, imposter_port, stub_id.map(str::to_owned))?;

    // Set global variables
    let global = context.global_object();
    global
        .set(js_string!("__request"), request_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set request: {e}"))?;
    global
        .set(js_string!("__config"), config_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set config: {e}"))?;
    global
        .set(
            js_string!("__response"),
            JsValue::from(response_obj),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set response: {e}"))?;
    global
        .set(js_string!("__logger"), logger_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set logger: {e}"))?;
    global
        .set(
            js_string!("__state"),
            state_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set state: {e}"))?;

    // Wrap the decorate function to call it with (config, response, logger, state). `config`
    // stands in for the legacy `request` positional arg (request fields flattened onto it), so
    // both `function(request, response, ...)` and `function(config, response, ...)` scripts work.
    let wrapper_script = format!(
        r#"
        var __decorateFn = {decorate_fn};
        __decorateFn(__config, __response, __logger, __state);
        __response;
        "#
    );

    // Execute the script
    let result = context
        .eval(Source::from_bytes(wrapper_script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute decorate function: {e}"))?;

    // Persist any mutation the decorate made to the shared, per-port state.
    if let Ok(Value::Object(map)) = js_to_json(&mut context, &state_obj) {
        save_imposter_state(imposter_port, map);
    }

    // Parse the modified response
    let obj = result
        .as_object()
        .ok_or_else(|| anyhow!("Decorate function must return response object"))?;

    // Get statusCode
    let status_code = obj
        .get(js_string!("statusCode"), &mut context)
        .ok()
        .and_then(|v| v.as_number())
        .map(|n| n as u16)
        .unwrap_or(response_status);

    // Get headers
    let mut headers = response_headers.clone();
    if let Ok(headers_val) = obj.get(js_string!("headers"), &mut context)
        && let Some(headers_obj) = headers_val.as_object()
        && let Ok(keys) = headers_obj.own_property_keys(&mut context)
    {
        for key in keys {
            let key_str = match &key {
                PropertyKey::String(s) => s.to_std_string_escaped(),
                PropertyKey::Index(i) => i.get().to_string(),
                PropertyKey::Symbol(_) => continue,
            };
            if let Ok(val) = headers_obj.get(key.clone(), &mut context)
                && let Some(s) = val.as_string()
            {
                headers.insert(key_str, s.to_std_string_escaped());
            }
        }
    }

    // Get body
    let body = obj
        .get(js_string!("body"), &mut context)
        .ok()
        .map(|v| {
            if let Some(s) = v.as_string() {
                s.to_std_string_escaped()
            } else if v.is_object() {
                let json = js_to_json(&mut context, &v).unwrap_or(Value::Null);
                serde_json::to_string(&json).unwrap_or_default()
            } else if v.is_null() || v.is_undefined() {
                String::new()
            } else {
                v.display().to_string()
            }
        })
        .unwrap_or_else(|| response_body.to_string());

    Ok(MountebankDecorateResponse {
        status_code,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InMemoryFlowStore;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_js_engine_compiles() {
        let script = r#"
function respond(ctx) {
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_js_engine_without_should_inject_still_constructs() {
        // Issue #357 Item 4: a script defining no v2 named entrypoint is legal — it's a
        // bare-expression script (Item 2). Construction only validates that the script compiles.
        let script = r#"
function some_other_function() {
    return true;
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(
            engine.is_ok(),
            "bare/helper-only scripts must construct: {:?}",
            engine.err()
        );
    }

    #[test]
    fn test_js_engine_syntax_error_fails_construction() {
        let script = "function respond(ctx {  // missing paren";
        let engine = JsEngine::new(script, "test-rule");
        assert!(
            engine.is_err(),
            "a genuine syntax error must still fail construction"
        );
    }

    #[test]
    fn test_js_simple_fault_injection() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/api/test") {
        return http(503, "Service unavailable");
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 503);
                assert_eq!(body, "Service unavailable");
            }
            _ => panic!("Expected error fault decision"),
        }
    }

    // Issue #327: the JS respond(ctx) Boa context caps loop iterations so a runaway script
    // terminates (Boa throws) instead of leaking its spawn_blocking thread forever. A small
    // injected limit keeps these tests fast and guarantees they can never hang.
    #[test]
    fn js_respond_loop_limit_terminates() {
        set_current_flow_store(Arc::new(InMemoryFlowStore::new(300)));
        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let script = "function respond(ctx) { while (true) {} }";
        let ctx_input = ScriptCtxExtras::default().build_ctx_input(&request);
        let result = execute_js_script_inner_bounded(script, "rule", &ctx_input, 100_000);
        clear_current_flow_store();
        assert!(
            result.is_err(),
            "a runaway JS loop must hit the iteration cap and return Err, not run unbounded"
        );
    }

    #[test]
    fn js_respond_under_limit_runs() {
        set_current_flow_store(Arc::new(InMemoryFlowStore::new(300)));
        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let script = "function respond(ctx) { let n = 0; for (let i = 0; i < 100; i++) { n++; } return n === 100 ? delay(1) : pass(); }";
        let ctx_input = ScriptCtxExtras::default().build_ctx_input(&request);
        let result = execute_js_script_inner_bounded(script, "rule", &ctx_input, 100_000);
        clear_current_flow_store();
        assert!(
            result.is_ok(),
            "a small-loop script under the cap must run fine: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_js_latency_fault() {
        let script = r#"
function respond(ctx) {
    return delay(1000);
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Latency { duration_ms, .. } => {
                assert_eq!(duration_ms, 1000);
            }
            _ => panic!("Expected latency fault decision"),
        }
    }

    #[test]
    fn test_js_flow_store_increment() {
        let script = r#"
function respond(ctx) {
    var attempts = ctx.state.incr("attempts");

    if (attempts <= 2) {
        return http(503, "Attempt " + attempts);
    }

    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow-123".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        // First attempt should inject fault
        let result1 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result1, FaultDecision::Error { .. }));

        // Second attempt should inject fault
        let result2 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result2, FaultDecision::Error { .. }));

        // Third attempt should not inject fault
        let result3 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result3, FaultDecision::None));
    }

    #[test]
    fn test_js_flow_store_get_set() {
        let script = r#"
function respond(ctx) {
    // Set a value
    ctx.state.set("test_key", "test_value");

    // Get it back
    var value = ctx.state.get("test_key");

    // Check if it matches
    if (value === "test_value") {
        return http(200, "Get/Set works!");
    }

    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow-123".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        // Should inject fault if get/set works
        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "Get/Set works!");
            }
            _ => panic!("Expected error fault decision - get/set failed"),
        }
    }

    #[test]
    fn test_compile_js_to_bytecode() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/test") {
        return http(500, "Test error");
    }
    return pass();
}
"#;

        // Compile to bytecode
        let bytecode = compile_js_to_bytecode(script).unwrap();

        // Verify bytecode is not empty
        assert!(!bytecode.is_empty());
    }

    #[test]
    fn test_execute_js_bytecode() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/api/bytecode") {
        return http(503, "Bytecode executed");
    }
    return pass();
}
"#;

        // Compile to bytecode
        let bytecode = compile_js_to_bytecode(script).unwrap();

        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/bytecode".to_string(),
            headers,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = execute_js_bytecode(&bytecode, &request, store, "bytecode-rule").unwrap();

        match result {
            FaultDecision::Error {
                status,
                body,
                rule_id,
                headers,
            } => {
                assert_eq!(status, 503);
                assert_eq!(body, "Bytecode executed");
                assert_eq!(rule_id, "bytecode-rule");
                assert!(headers.is_empty());
            }
            _ => panic!("Expected error fault decision from bytecode execution"),
        }
    }

    #[test]
    fn test_js_with_complex_body() {
        let script = r#"
function respond(ctx) {
    var body = ctx.request.json;
    if (body && body.nested && body.nested.value > 100) {
        return http(400, "Value too high: " + body.nested.value);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Test with high value
        let request1 = ScriptRequest {
            raw_body: None,
            method: "POST".to_string(),
            path: "/api/test".to_string(),
            headers: HashMap::new(),
            body: json!({
                "nested": {
                    "value": 200,
                    "name": "test"
                },
                "array": [1, 2, 3]
            }),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result1 = engine.should_inject(&request1, Arc::clone(&store)).unwrap();

        match result1 {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 400);
                assert_eq!(body, "Value too high: 200");
            }
            _ => panic!("Expected error for high value"),
        }

        // Test with low value
        let request2 = ScriptRequest {
            raw_body: None,
            method: "POST".to_string(),
            path: "/api/test".to_string(),
            headers: HashMap::new(),
            body: json!({
                "nested": {
                    "value": 50,
                    "name": "test"
                },
                "array": [4, 5, 6]
            }),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result2 = engine.should_inject(&request2, store).unwrap();
        assert!(matches!(result2, FaultDecision::None));
    }

    #[test]
    fn test_js_error_with_headers() {
        let script = r#"
function respond(ctx) {
    return http(502, "Gateway error")
        .header("X-Custom-Header", "custom-value")
        .header("X-Error-Code", "E001");
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error {
                status, headers, ..
            } => {
                assert_eq!(status, 502);
                assert_eq!(
                    headers.get("X-Custom-Header"),
                    Some(&"custom-value".to_string())
                );
                assert_eq!(headers.get("X-Error-Code"), Some(&"E001".to_string()));
            }
            _ => panic!("Expected error fault decision with headers"),
        }
    }

    #[test]
    fn test_js_query_params() {
        let script = r#"
function respond(ctx) {
    var name = ctx.request.query["name"];
    var page = ctx.request.query["page"];

    if (name && page) {
        return http(200, "Hello " + name + " on page " + page);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut query = HashMap::new();
        query.insert("name".to_string(), "Alice".to_string());
        query.insert("page".to_string(), "42".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query,
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "Hello Alice on page 42");
            }
            _ => panic!("Expected error fault decision with query params"),
        }
    }

    #[test]
    fn test_js_path_params() {
        let script = r#"
function respond(ctx) {
    var user_id = ctx.request.pathParams["id"];
    var action = ctx.request.pathParams["action"];

    if (user_id && action) {
        return http(200, "User " + user_id + " action: " + action);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "123".to_string());
        path_params.insert("action".to_string(), "update".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "POST".to_string(),
            path: "/users/123/update".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params,
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "User 123 action: update");
            }
            _ => panic!("Expected error fault decision with path params"),
        }
    }

    // Allocates a fresh, never-reused port number for each test that touches the process-global
    // `IMPOSTER_STATE` map, so parallel tests never see each other's persisted state.
    fn test_port() -> u16 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(40_100);
        NEXT.fetch_add(1, Ordering::Relaxed) as u16
    }

    #[test]
    fn test_decorate_with_logger_arg() {
        let request = MountebankRequest {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };
        let headers = HashMap::new();

        // Script that uses logger as 3rd argument — must not throw ReferenceError
        let script = r#"function(request, response, logger) { logger.info("decorating"); response.body = "logged"; }"#;
        let result = execute_mountebank_decorate(
            script,
            &request,
            "original",
            200,
            &headers,
            test_port(),
            None,
        );
        assert!(
            result.is_ok(),
            "logger arg should not throw: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().body, "logged");
    }

    #[test]
    fn test_decorate_with_state_arg() {
        let request = MountebankRequest {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };
        let headers = HashMap::new();

        // Script that uses state as 4th argument — must not throw ReferenceError
        let script = r#"function(request, response, logger, state) { state.count = 1; response.body = "state ok"; }"#;
        let result = execute_mountebank_decorate(
            script,
            &request,
            "original",
            200,
            &headers,
            test_port(),
            None,
        );
        assert!(
            result.is_ok(),
            "state arg should not throw: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().body, "state ok");
    }

    // Issue #305 gate: `config =>` decorate convention in Boa + CommonJS require().
    fn config_req() -> MountebankRequest {
        MountebankRequest {
            method: "POST".to_string(),
            path: "/req".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some("REQ-BODY".to_string()),
        }
    }

    // Writes a unique temp .cjs module and returns its path; caller removes it.
    fn write_temp_cjs(tag: &str, source: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rift_305_{tag}_{}_{n}.cjs", std::process::id()));
        std::fs::write(&path, source).expect("write temp module");
        path
    }

    // AC1: require() loads + runs an external module; its config.response mutation takes effect.
    #[test]
    fn test_config_decorate_require_runs_module() {
        let module = write_temp_cjs(
            "run",
            "module.exports = function (config) {\n  config.response.body = 'REQUIRE-RAN';\n  config.response.headers = config.response.headers || {};\n  config.response.headers['X-Injected-By'] = 'mod.cjs';\n};\n",
        );
        let script = format!(
            "config => {{ const s = require('{}'); s(config); }}",
            module.display()
        );
        let result = execute_mountebank_config_decorate(
            &script,
            &config_req(),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        );
        let _ = std::fs::remove_file(&module);
        let resp = result.expect("require decorate should run");
        assert_eq!(resp.body, "REQUIRE-RAN");
        assert_eq!(
            resp.headers.get("X-Injected-By").map(String::as_str),
            Some("mod.cjs")
        );
    }

    // AC2: the config convention runs in Boa without require.
    #[test]
    fn test_config_decorate_direct_field_access() {
        let script = "config => { config.response.body = 'DIRECT'; }";
        let resp = execute_mountebank_config_decorate(
            script,
            &config_req(),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("direct config decorate should run");
        assert_eq!(resp.body, "DIRECT");
    }

    // AC3: config.request is exposed to the decorate.
    #[test]
    fn test_config_decorate_reads_request_body() {
        let script = "config => { config.response.body = config.request.body; }";
        let resp = execute_mountebank_config_decorate(
            script,
            &config_req(),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("config.request decorate should run");
        assert_eq!(resp.body, "REQ-BODY");
    }

    // Circular require() must terminate (Node cycle-break), not recurse into a stack overflow.
    #[test]
    fn test_config_decorate_require_circular_terminates() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path_a = dir.join(format!("rift_305_cyc_a_{pid}_{n}.cjs"));
        let path_b = dir.join(format!("rift_305_cyc_b_{pid}_{n}.cjs"));
        // a <-> b require each other (the cycle); b gets a's partial exports via the cache instead
        // of recursing forever. The outer require(a) must still return a's FINAL exports.
        std::fs::write(
            &path_a,
            format!(
                "const b = require('{}');\nmodule.exports = {{ tag: 'A-LOADED' }};\n",
                path_b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &path_b,
            format!(
                "const a = require('{}');\nmodule.exports = {{ tag: 'B-LOADED' }};\n",
                path_a.display()
            ),
        )
        .unwrap();
        // The guarantee is termination: require(a) drives the a->b->a cycle, which the cache-break
        // resolves instead of recursing until the native stack overflows (which would abort the
        // process). Execution then continues and sets the body. (Exact exports of a module observed
        // mid-cycle are CommonJS-implementation-defined and out of scope here.)
        let script = format!(
            "config => {{ require('{}'); config.response.body = 'REACHED'; }}",
            path_a.display()
        );
        let result = execute_mountebank_config_decorate(
            &script,
            &config_req(),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        );
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        assert_eq!(
            result
                .expect("circular require must terminate, not overflow")
                .body,
            "REACHED"
        );
    }

    // AC4: a require() of a missing module surfaces as an error, not a silent Ok/no-op.
    #[test]
    fn test_config_decorate_require_missing_errors() {
        let script = "config => { const s = require('/no/such/rift305/module.cjs'); s(config); }";
        let result = execute_mountebank_config_decorate(
            script,
            &config_req(),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        );
        assert!(
            result.is_err(),
            "a missing require() must surface as Err, not silently no-op"
        );
    }

    // =========================================================================================
    // Issue #355 Item 0: Mountebank v2 `config` calling convention (dual-convention, zero
    // breakage) for response inject, predicate inject, and decorate.
    // =========================================================================================

    fn mb_req(method: &str, path: &str) -> MountebankRequest {
        MountebankRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        }
    }

    // (a) v2 config convention: function(config) { ...config.request.path... }
    #[test]
    fn inject_v2_config_request_path() {
        let script =
            r#"function(config) { return { statusCode: 200, body: config.request.path }; }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/a-path"), test_port(), None)
            .expect("v2 config convention should run");
        assert_eq!(resp.body, "/a-path");
    }

    // (b) legacy positional: function(request, state, logger) { ...request.path... }
    #[test]
    fn inject_legacy_positional_request_path() {
        let script = r#"function(request, state, logger) { return { statusCode: 200, body: request.path }; }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/legacy"), test_port(), None)
            .expect("legacy positional convention should run");
        assert_eq!(resp.body, "/legacy");
    }

    // (c) flattened: function(config) { ...config.path... } (no config.request access)
    #[test]
    fn inject_flattened_config_path() {
        let script = r#"function(config) { return { statusCode: 200, body: config.path + " " + config.method }; }"#;
        let resp = execute_mountebank_inject(script, &mb_req("POST", "/flat"), test_port(), None)
            .expect("flattened config fields should be readable");
        assert_eq!(resp.body, "/flat POST");
    }

    // (d) the `var req = config.request || config;` shim used by scripts written to run under
    // either the old or new convention.
    #[test]
    fn inject_request_or_config_shim() {
        let script = r#"function(config) {
            var req = config.request || config;
            return { statusCode: 200, body: req.path };
        }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/shim"), test_port(), None)
            .expect("request-or-config shim should run");
        assert_eq!(resp.body, "/shim");
    }

    // (e) hybrid: function(config, state) { ... } — a script naming the 2nd positional param
    // "state" (matching Mountebank's legacy name) still works positionally.
    #[test]
    fn inject_hybrid_config_state() {
        let script = r#"function(config, state) {
            state.hits = (state.hits || 0) + 1;
            return { statusCode: 200, body: "hits:" + state.hits };
        }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/hybrid"), test_port(), None)
            .expect("hybrid config+state signature should run");
        assert_eq!(resp.body, "hits:1");
    }

    // The async-callback convention: `done` (arg 4) is called instead of returning a value;
    // `config.callback` is an alias of the same function, so either spelling works.
    #[test]
    fn inject_callback_style_done() {
        let script = r#"function(config, injectState, logger, done) {
            done({ statusCode: 200, body: "via-done" });
        }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/cb"), test_port(), None)
            .expect("callback-style inject should run");
        assert_eq!(resp.body, "via-done");
    }

    #[test]
    fn inject_callback_style_config_callback_alias() {
        let script = r#"function(config) {
            config.callback({ statusCode: 200, body: "via-config-callback" });
        }"#;
        let resp = execute_mountebank_inject(script, &mb_req("GET", "/cb2"), test_port(), None)
            .expect("config.callback alias should run");
        assert_eq!(resp.body, "via-config-callback");
    }

    // Predicate inject: same five conventions, adapted to a boolean-returning function.
    #[test]
    fn predicate_v2_config_request_path() {
        let script = r#"function(config) { return config.request.path === "/match"; }"#;
        assert!(execute_predicate_inject(
            script,
            &mb_req("GET", "/match"),
            test_port()
        ));
    }

    #[test]
    fn predicate_legacy_positional() {
        let script = r#"function(request, logger, state) { return request.path === "/match"; }"#;
        assert!(execute_predicate_inject(
            script,
            &mb_req("GET", "/match"),
            test_port()
        ));
    }

    #[test]
    fn predicate_flattened_config_path() {
        let script = r#"function(config) { return config.path === "/match"; }"#;
        assert!(execute_predicate_inject(
            script,
            &mb_req("GET", "/match"),
            test_port()
        ));
    }

    #[test]
    fn predicate_request_or_config_shim() {
        let script = r#"function(config) {
            var req = config.request || config;
            return req.path === "/match";
        }"#;
        assert!(execute_predicate_inject(
            script,
            &mb_req("GET", "/match"),
            test_port()
        ));
    }

    #[test]
    fn predicate_hybrid_config_state() {
        let script = r#"function(config, state) {
            state.checked = true;
            return config.method === "GET";
        }"#;
        assert!(execute_predicate_inject(
            script,
            &mb_req("GET", "/match"),
            test_port()
        ));
    }

    // Decorate: same five conventions.
    #[test]
    fn decorate_v2_config_request_path() {
        let script = r#"function(config, response) { response.body = config.request.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-a"),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("v2 config decorate should run");
        assert_eq!(resp.body, "/dec-a");
    }

    #[test]
    fn decorate_legacy_positional() {
        let script = r#"function(request, response, logger) { response.body = request.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-legacy"),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("legacy positional decorate should run");
        assert_eq!(resp.body, "/dec-legacy");
    }

    #[test]
    fn decorate_flattened_config_path() {
        let script = r#"function(config, response) { response.body = config.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-flat"),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("flattened config decorate should run");
        assert_eq!(resp.body, "/dec-flat");
    }

    #[test]
    fn decorate_request_or_config_shim() {
        let script = r#"function(config, response) {
            var req = config.request || config;
            response.body = req.path;
        }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-shim"),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("request-or-config shim decorate should run");
        assert_eq!(resp.body, "/dec-shim");
    }

    #[test]
    fn decorate_hybrid_config_state() {
        let script = r#"function(config, response, logger, state) {
            state.decorated = true;
            response.body = "decorated:" + config.method;
        }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-hybrid"),
            "orig",
            200,
            &HashMap::new(),
            test_port(),
            None,
        )
        .expect("hybrid config+state decorate should run");
        assert_eq!(resp.body, "decorated:GET");
    }

    // State written by a predicate inject must be visible to a later response inject on the same
    // imposter port (issue #355 Item 0: config.state / imposterState is shared per-port).
    #[test]
    fn state_shared_between_predicate_and_response_inject() {
        let port = test_port();
        let predicate_script =
            r#"function(config) { config.state.seen = "from-predicate"; return true; }"#;
        assert!(execute_predicate_inject(
            predicate_script,
            &mb_req("GET", "/shared"),
            port
        ));

        let inject_script = r#"function(config) { return { statusCode: 200, body: config.state.seen || "missing" }; }"#;
        let resp = execute_mountebank_inject(inject_script, &mb_req("GET", "/shared"), port, None)
            .expect("response inject should run");
        assert_eq!(
            resp.body, "from-predicate",
            "state written by a predicate inject must be visible to a later response inject \
             on the same imposter port"
        );
    }

    // =========================================================================================
    // Issue #355 Item 1: native logger routes to `tracing`.
    // =========================================================================================

    /// Minimal capturing `tracing::Subscriber` (no external test-capture crate needed): records
    /// every event's target + fields as one formatted string per event.
    struct CapturingSubscriber {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={:?} ", field.name(), value));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push_str(&format!("{}={} ", field.name(), value));
                }
            }
            let mut visitor = Visitor(format!("target={} ", event.metadata().target()));
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // The native logger's methods are callable without throwing, AND the messages actually reach
    // the process's tracing subscriber at target "rift::script", tagged with the imposter port and
    // — when the caller provides one — the stub id (issue #355 Item 1 + AC1).
    #[test]
    fn mb_logger_routes_to_tracing() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: Arc::clone(&events),
        };
        let port = test_port();

        let script = r#"function(config) {
            config.logger.debug("dbg-msg");
            config.logger.info("hello-from-script");
            config.logger.warn("warn-msg");
            config.logger.error("err-msg");
            return { statusCode: 200, body: "logged" };
        }"#;

        let result = tracing::subscriber::with_default(subscriber, || {
            execute_mountebank_inject(script, &mb_req("GET", "/log"), port, Some("stub-log-42"))
        });

        assert!(
            result.is_ok(),
            "logger calls must not throw: {:?}",
            result.err()
        );

        let logs = events.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.contains("rift::script") && l.contains("hello-from-script")),
            "expected an info event carrying the script's message, got: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l.contains(&port.to_string())),
            "expected the imposter port to be tagged on a logged event, got: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l.contains("stub-log-42")),
            "expected the stub id to be tagged on a logged event when provided, got: {logs:?}"
        );
    }

    // =========================================================================================
    // Issue #355 Item 3: Boa RuntimeLimits on every MB JS hook Context.
    // =========================================================================================

    // An inject fn with a genuine infinite loop must terminate (Err), not hang, because
    // `bounded_js_context()` caps loop iterations on every Context these hooks build.
    #[test]
    fn mb_inject_infinite_loop_terminates() {
        let script = r#"function(config) { while (true) {} }"#;
        let result = execute_mountebank_inject(script, &mb_req("GET", "/loop"), test_port(), None);
        assert!(
            result.is_err(),
            "an infinite loop in an inject fn must terminate with an error, not hang"
        );
    }

    // =========================================================================================
    // Issue #355 Item 5 (AC5): a throwing inject fn surfaces as an Err (the handler maps this to
    // a Mountebank-shaped 400 — see issue_355_inject_error_parity.rs for the end-to-end assertion).
    // =========================================================================================
    #[test]
    fn mb_inject_throwing_returns_err() {
        let script = r#"function(config) { throw new Error('boom-inject'); }"#;
        let result = execute_mountebank_inject(script, &mb_req("GET", "/throw"), test_port(), None);
        let err = result.expect_err("a throwing inject must surface as Err, not silently succeed");
        assert!(
            err.to_string().contains("boom-inject"),
            "the error must carry the script's failure message, got: {err}"
        );
    }

    // ============================================
    // Issue #357: unified ctx, v2 respond entrypoint, result constructors (JS)
    // ============================================
    mod v2 {
        use super::*;

        fn req(headers: HashMap<String, String>, raw_body: Option<&str>) -> ScriptRequest {
            ScriptRequest {
                method: "POST".to_string(),
                path: "/api/orders".to_string(),
                headers,
                body: json!(null),
                query: HashMap::new(),
                path_params: HashMap::new(),
                raw_body: raw_body.map(|s| s.to_string()),
            }
        }

        fn store() -> Arc<dyn FlowStore> {
            Arc::new(InMemoryFlowStore::new(300))
        }

        fn run_respond(script: &str, request: &ScriptRequest) -> Result<FaultDecision> {
            let engine = JsEngine::new(script, "v2-rule").unwrap();
            engine.should_inject(request, store())
        }

        #[test]
        fn respond_named_wrapper() {
            let script = r#"
                function respond(ctx) {
                    return http(503, { error: "unavailable" });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error {
                    status,
                    body,
                    headers,
                    ..
                } => {
                    assert_eq!(status, 503);
                    assert!(body.contains("unavailable"));
                    assert_eq!(
                        headers.get("Content-Type").map(String::as_str),
                        Some("application/json")
                    );
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn respond_bare_expression() {
            let script = r#"
                (ctx.request.method === "POST") ? http(503, { error: "unavailable" }) : pass()
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        #[test]
        fn ctx_request_header_case_insensitive_and_lowercased_map() {
            let headers = HashMap::from([("X-Flow-Id".to_string(), "flow-9".to_string())]);
            let script = r#"
                function respond(ctx) {
                    var byGetter = ctx.request.header("x-flow-id");
                    var byMap = ctx.request.headers["x-flow-id"];
                    return http(200, { getter: byGetter, map: byMap });
                }
            "#;
            let decision = run_respond(script, &req(headers, None)).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("flow-9"));
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_json_lazy_parse() {
            let script = r#"
                function respond(ctx) {
                    if (ctx.request.json === null) { return http(500); }
                    return http(200, { n: ctx.request.json.n });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), Some(r#"{"n": 7}"#))).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains('7'));
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }

            let decision2 = run_respond(script, &req(HashMap::new(), Some("not json"))).unwrap();
            assert!(matches!(
                decision2,
                FaultDecision::Error { status: 500, .. }
            ));
        }

        #[test]
        fn result_constructors_delay_reset_pass() {
            let delay = run_respond(
                "function respond(ctx) { return delay(42); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            match delay {
                FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 42),
                other => panic!("expected Latency, got {other:?}"),
            }

            let reset = run_respond(
                "function respond(ctx) { return reset(); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(reset, FaultDecision::Reset { .. }));

            let pass = run_respond(
                "function respond(ctx) { return pass(); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(pass, FaultDecision::None));

            let nothing =
                run_respond("function respond(ctx) { }", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(nothing, FaultDecision::None));
        }

        #[test]
        fn http_string_body_passes_through_verbatim() {
            let script = r#"function respond(ctx) { return http(200, "hello world"); }"#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, headers, .. } => {
                    assert_eq!(body, "hello world");
                    assert!(!headers.contains_key("Content-Type"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        // Issue #453: v1 `should_inject` was removed — a script defining only `should_inject`
        // (and no `respond`) is now just a misnamed entrypoint, not a special v1 detection.
        #[test]
        fn should_inject_only_script_is_misnamed_entrypoint_error() {
            let script = r#"
                function should_inject(request, flow_store) {
                    return { inject: true, fault: "error", status: 418, body: "teapot" };
                }
            "#;
            let err = run_respond(script, &req(HashMap::new(), None)).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("entrypoint") && msg.contains("respond"),
                "expected the standard misnamed-entrypoint error naming `respond`, got: {msg}"
            );
        }

        // Retry example from the issue: ctx.state.incr + http() executes end-to-end.
        #[test]
        fn retry_example_end_to_end_with_ctx_state_incr() {
            let script = r#"
                function respond(ctx) {
                    var n = ctx.state.incr("attempts");
                    if (n <= 2) {
                        return http(503, { error: "unavailable", attempt: n }).header("Retry-After", "1");
                    }
                    return http(200, { ok: true, succeededOnAttempt: n });
                }
            "#;
            let engine = JsEngine::new(script, "retry-rule").unwrap();
            let shared_store = store();
            let request = req(HashMap::new(), None);

            let d1 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d1, FaultDecision::Error { status: 503, .. }));

            let d2 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d2, FaultDecision::Error { status: 503, .. }));

            let d3 = engine.should_inject(&request, shared_store).unwrap();
            match d3 {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("succeededOnAttempt"));
                }
                other => panic!("expected 200 on third attempt, got {other:?}"),
            }
        }

        // --- ctx.state atomic ops (issue #358) ---

        #[test]
        fn get_or_returns_default_when_absent_then_stored_value() {
            let script = r#"
                function respond(ctx) {
                    var first = ctx.state.getOr("count", 0);
                    ctx.state.set("count", 7);
                    var second = ctx.state.getOr("count", 0);
                    return http(200, { first: first, second: second });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"first\":0"), "got {body}");
                    assert!(body.contains("\"second\":7"), "got {body}");
                }
                other => panic!("expected Error(200), got {other:?}"),
            }
        }

        #[test]
        fn incr_by_is_atomic_and_starts_at_zero() {
            let script = r#"
                function respond(ctx) {
                    var n = ctx.state.incrBy("hits", 5);
                    return http(200, { n: n });
                }
            "#;
            let engine = JsEngine::new(script, "incr-by-rule").unwrap();
            let shared_store = store();
            let request = req(HashMap::new(), None);

            let d1 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            match d1 {
                FaultDecision::Error { body, .. } => assert!(body.contains("\"n\":5")),
                other => panic!("expected Error(200), got {other:?}"),
            }

            let d2 = engine.should_inject(&request, shared_store).unwrap();
            match d2 {
                FaultDecision::Error { body, .. } => assert!(body.contains("\"n\":10")),
                other => panic!("expected Error(200), got {other:?}"),
            }
        }

        #[test]
        fn cas_distinguishes_applied_from_conflict() {
            let shared_store = store();
            let request = req(HashMap::new(), None);

            // First call: key "status" absent, expected null matches -> applied, sets "paid".
            let applied_script = r#"
                function respond(ctx) {
                    var outcome = ctx.state.cas("status", null, "paid");
                    return http(200, { applied: outcome.applied, current: outcome.current });
                }
            "#;
            let applied_engine = JsEngine::new(applied_script, "cas-applied").unwrap();
            let d1 = applied_engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            match d1 {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":true"), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }

            // Second call: current is now "paid"; expecting "pending" must conflict and report the
            // winning current value, distinguishing it from the Applied case above.
            let conflict_script = r#"
                function respond(ctx) {
                    var outcome = ctx.state.cas("status", "pending", "shipped");
                    return http(200, { applied: outcome.applied, current: outcome.current });
                }
            "#;
            let conflict_engine = JsEngine::new(conflict_script, "cas-conflict").unwrap();
            let d2 = conflict_engine
                .should_inject(&request, shared_store)
                .unwrap();
            match d2 {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":false"), "got {body}");
                    assert!(body.contains("\"current\":\"paid\""), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        #[test]
        fn ttl_sets_per_flow_expiry() {
            let script = r#"
                function respond(ctx) {
                    ctx.state.set("k", 1);
                    var applied = ctx.state.ttl(3600);
                    return http(200, { applied: applied });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":true"), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // Issue #358 B2: JS has one number type, so `set("count", 5)` arrives as f64 5.0. The
        // js_to_json integer fix stores it as an integer so a following incr_by/incr accumulates
        // instead of silently restarting from 0.
        #[test]
        fn set_whole_number_then_incr_by_accumulates() {
            let script = r#"
                function respond(ctx) {
                    ctx.state.set("count", 5);
                    var a = ctx.state.incrBy("count", 1);
                    var b = ctx.state.incr("count");
                    return http(200, { a: a, b: b });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(
                        body.contains("\"a\":6"),
                        "expected incr_by to yield 6, got {body}"
                    );
                    assert!(
                        body.contains("\"b\":7"),
                        "expected incr to yield 7, got {body}"
                    );
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // Issue #358 B3 (AC4): a backend failure on any atomic op must propagate as a script error.
        #[cfg(feature = "test-backend")]
        #[test]
        fn atomic_ops_propagate_store_errors_fail_loud() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            for (name, script) in [
                (
                    "get_or",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.getOr("k", 0) }); }"#,
                ),
                (
                    "incr_by",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.incrBy("k", 1) }); }"#,
                ),
                (
                    "cas",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.cas("k", null, "v").applied }); }"#,
                ),
                (
                    "ttl",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.ttl(60) }); }"#,
                ),
            ] {
                let result = JsEngine::new(script, "fail")
                    .unwrap()
                    .should_inject(&request, Arc::new(FailingFlowStore));
                assert!(result.is_err(), "{name} must propagate a store failure");
            }
        }

        // Issue #358 B1 / #322: the ctx.state ops (get/incr) fail loud even with the toggle unset.
        #[cfg(feature = "test-backend")]
        #[test]
        fn v2_state_ops_fail_loud_regardless_of_strict_toggle() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            let get_err = JsEngine::new(
                r#"function respond(ctx) { return http(200, { v: ctx.state.get("k") }); }"#,
                "v2-get",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(get_err.is_err(), "v2 ctx.state.get must fail loud");

            let incr_err = JsEngine::new(
                r#"function respond(ctx) { return http(200, { v: ctx.state.incr("k") }); }"#,
                "v2-incr",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(incr_err.is_err(), "v2 ctx.state.incr must fail loud");
        }

        // B1 (issue #357): a script defining ONLY a misnamed entrypoint must Err, not None.
        #[test]
        fn misnamed_entrypoint_errors_not_none() {
            let script = r#"
                function respnod(ctx) {
                    return http(500);
                }
            "#;
            let result = JsEngine::new(script, "v2-rule")
                .unwrap()
                .should_inject(&req(HashMap::new(), None), store());
            assert!(
                result.is_err(),
                "a misnamed entrypoint must surface an error, got {result:?}"
            );
        }

        #[test]
        fn genuine_bare_expression_still_ok_after_b1() {
            let decision = run_respond("http(503)", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        // ctx.request.pathParams and ctx.request.query round-trip (mirrors the Rhai test).
        #[test]
        fn ctx_request_path_params_and_query() {
            let mut request = req(HashMap::new(), None);
            request
                .path_params
                .insert("id".to_string(), "42".to_string());
            request.query.insert("page".to_string(), "2".to_string());
            let script = r#"
                function respond(ctx) {
                    return http(200, { id: ctx.request.pathParams.id, page: ctx.request.query.page });
                }
            "#;
            let decision = run_respond(script, &request).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("42"), "pathParams.id missing: {body}");
                    assert!(body.contains('2'), "query.page missing: {body}");
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        // B3 (issue #357): the ScriptResult registry must not grow unbounded across runs on a
        // reused worker thread. Each execution makes constructor calls that are NOT the returned
        // value (an orphaned `http(999)` completion), yet the registry is reset per run, so its
        // size stays bounded (here: empty at the start of each run, so <= 1 entry mid-run).
        #[test]
        fn script_result_registry_does_not_leak_across_runs() {
            let script = "http(999); pass()"; // orphaned http(999), returns pass()
            for _ in 0..25 {
                let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
                assert!(matches!(decision, FaultDecision::None));
            }
            let leaked = SCRIPT_RESULT_REGISTRY.with(|r| r.borrow().len());
            assert!(
                leaked <= 1,
                "registry leaked {leaked} entries across 25 runs; per-run reset is broken"
            );
        }
    }
}
