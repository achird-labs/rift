use crate::extensions::flow_state::FlowStore;
use anyhow::{Result, anyhow};
use rhai::{AST, Dynamic, Engine, Map, Scope};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    FaultDecision, ScriptCtxExtras, ScriptCtxInput, ScriptFlowStore, ScriptRequest,
    ScriptResponseContext, ScriptResult, ScriptResultBody, entrypoints,
};

/// Helper function to check if a year is a leap year
fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Create a Rhai Map from a ScriptRequest
fn create_request_map(request: &ScriptRequest) -> Map {
    let mut request_map = Map::new();
    request_map.insert("method".into(), Dynamic::from(request.method.clone()));
    request_map.insert("path".into(), Dynamic::from(request.path.clone()));

    // Convert headers
    let mut headers_map = Map::new();
    for (k, v) in &request.headers {
        headers_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    request_map.insert("headers".into(), Dynamic::from(headers_map));

    // Convert query parameters
    let mut query_map = Map::new();
    for (k, v) in &request.query {
        query_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    request_map.insert("query".into(), Dynamic::from(query_map));

    // Convert path parameters
    let mut path_params_map = Map::new();
    for (k, v) in &request.path_params {
        path_params_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    request_map.insert("pathParams".into(), Dynamic::from(path_params_map));

    // Convert body
    request_map.insert("body".into(), json_to_dynamic(request.body.clone()));

    request_map
}

/// Rhai script engine for fault injection
///
/// # Script Interface
///
/// Scripts must define a `should_inject` function with the following signature:
///
/// ```rhai
/// fn should_inject(request, flow_store) {
///     // Your logic here
///     return #{ inject: false };
/// }
/// ```
///
/// ## Request Object
///
/// The `request` parameter is a map containing:
/// - `method` - HTTP method (string): "GET", "POST", "PUT", "DELETE", etc.
/// - `path` - Request path (string): "/api/users/123"
/// - `headers` - Map of header name (string) to value (string)
/// - `body` - Request body (parsed JSON value or null)
/// - `query` - Map of query parameter name (string) to value (string)
/// - `pathParams` - Map of path parameters extracted from route patterns
///
/// ## Flow Store Object
///
/// The `flow_store` parameter provides state management across requests:
/// - `flow_store.get(flow_id, key)` - Get a stored value (returns unit if not found)
/// - `flow_store.set(flow_id, key, value)` - Store a value (returns bool)
/// - `flow_store.exists(flow_id, key)` - Check if key exists (returns bool)
/// - `flow_store.delete(flow_id, key)` - Delete a key (returns bool)
/// - `flow_store.increment(flow_id, key)` - Increment counter (returns i64)
/// - `flow_store.set_ttl(flow_id, ttl_seconds)` - Set flow expiration (returns bool)
/// - `flow_store.last_error()` - Take the last flow-store op's error (returns unit if none)
///
/// ## Return Value
///
/// The function must return a map with the fault decision:
///
/// ```rhai
/// // No fault injection
/// #{ inject: false }
///
/// // Latency injection
/// #{ inject: true, fault: "latency", duration_ms: 500 }
///
/// // Error injection
/// #{ inject: true, fault: "error", status: 503, body: "Service unavailable" }
///
/// // Error with custom headers
/// #{
///     inject: true,
///     fault: "error",
///     status: 429,
///     body: "Rate limited",
///     headers: #{ "Retry-After": "60" }
/// }
/// ```
///
/// ## Example
///
/// ```rhai
/// fn should_inject(request, flow_store) {
///     // Rate limit based on flow ID from header
///     let flow_id = request.headers["x-flow-id"];
///     if flow_id != () {
///         let attempts = flow_store.increment(flow_id, "attempts");
///         if attempts > 3 {
///             return #{ inject: true, fault: "error", status: 429, body: "Rate limited" };
///         }
///     }
///
///     // Inject fault for POST requests to specific path
///     if request.method == "POST" && request.path == "/api/test" {
///         return #{ inject: true, fault: "latency", duration_ms: 100 };
///     }
///
///     #{ inject: false }
/// }
/// ```
#[derive(Clone)]

pub struct RhaiEngine {
    ast: Arc<AST>, // Wrapped in Arc for efficient sharing with script pool
    rule_id: String,
}

impl RhaiEngine {
    pub fn new(script: &str, rule_id: &str) -> Result<Self> {
        let engine = Self::create_engine();
        let ast = engine
            .compile(script)
            .map_err(|e| anyhow!("Failed to compile script: {e}"))?;

        Ok(Self {
            ast: Arc::new(ast), // Wrap AST in Arc for sharing
            rule_id: rule_id.to_string(),
        })
    }

    /// Get a reference to the cached AST (for script pool)
    pub fn ast(&self) -> &Arc<AST> {
        &self.ast
    }

    /// Get the rule ID
    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn create_engine() -> Engine {
        let mut engine = Engine::new();

        // Register ScriptFlowStore type
        engine
            .register_type::<ScriptFlowStore>()
            .register_fn("get", ScriptFlowStore::get)
            .register_fn("set", ScriptFlowStore::set)
            .register_fn("exists", ScriptFlowStore::exists)
            .register_fn("delete", ScriptFlowStore::delete)
            .register_fn("increment", ScriptFlowStore::increment)
            .register_fn("set_ttl", ScriptFlowStore::set_ttl)
            .register_fn("last_error", ScriptFlowStore::last_error);

        // Register helper function for RFC 1123 timestamps
        engine.register_fn("timestamp_header", || -> String {
            // Generate RFC 1123 formatted timestamp for HTTP Date header
            // Format: "Tue, 13 Aug 2024 21:51:22 GMT"
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now();
            let duration = now.duration_since(UNIX_EPOCH).unwrap();
            let secs = duration.as_secs();

            // Convert to broken-down time
            let days_since_epoch = secs / 86400;
            let time_of_day = secs % 86400;
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;

            // Calculate day of week (epoch was Thursday)
            let day_of_week = (days_since_epoch + 4) % 7;
            let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

            // Calculate year, month, day
            let mut year = 1970;
            let mut remaining_days = days_since_epoch;
            loop {
                let days_in_year = if is_leap_year(year) { 366 } else { 365 };
                if remaining_days < days_in_year {
                    break;
                }
                remaining_days -= days_in_year;
                year += 1;
            }

            let months = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let days_in_months = [
                31,
                if is_leap_year(year) { 29 } else { 28 },
                31,
                30,
                31,
                30,
                31,
                31,
                30,
                31,
                30,
                31,
            ];

            let mut month = 0;
            let mut day = remaining_days + 1;
            for (i, &days_in_month) in days_in_months.iter().enumerate() {
                if day <= days_in_month {
                    month = i;
                    break;
                }
                day -= days_in_month;
            }

            format!(
                "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
                days[day_of_week as usize], day, months[month], year, hours, minutes, seconds
            )
        });

        register_v2_api(&mut engine);

        engine
    }

    /// Execute the script and determine if a fault should be injected. Auto-detects v1
    /// (`should_inject(request, flow_store)`) vs v2 (`respond(ctx)` or bare) — see
    /// [`ScriptEngine::should_inject_fault`] for the full contract.
    pub fn should_inject_fault(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        self.should_inject_fault_with_ctx(request, flow_store, &ScriptCtxExtras::default())
    }

    /// As [`Self::should_inject_fault`], but with real `ctx.flowId`/`ctx.stub` context (issue
    /// #357 Item 1).
    pub fn should_inject_fault_with_ctx(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        extra: &ScriptCtxExtras,
    ) -> Result<FaultDecision> {
        let engine = Self::create_engine();
        let ctx_input = extra.build_ctx_input(request);
        call_respond(
            &engine,
            self.ast.as_ref(),
            request,
            &ctx_input,
            flow_store,
            &self.rule_id,
        )
    }
}

/// Register the v2 `ctx` API (issue #357 Items 1/3) on an [`Engine`]: `ctx.state`/`ctx.store`
/// (flow-store handles), `ctx.logger`, the `http`/`delay`/`reset`/`pass` result constructors, and
/// a case-insensitive `.header(name)` getter usable on `ctx.request`/`ctx.response`. Shared by
/// `RhaiEngine::create_engine` so every caller (pooled, bounded, direct) gets the same API.
fn register_v2_api(engine: &mut Engine) {
    engine
        .register_type::<RhaiStateHandle>()
        .register_fn("get", RhaiStateHandle::get)
        .register_fn("set", RhaiStateHandle::set)
        .register_fn("incr", RhaiStateHandle::incr)
        .register_fn("exists", RhaiStateHandle::exists)
        .register_fn("delete", RhaiStateHandle::delete);

    engine
        .register_type::<RhaiStoreHandle>()
        .register_fn("flow", RhaiStoreHandle::flow);

    engine
        .register_type::<RhaiLogger>()
        .register_fn("debug", RhaiLogger::debug)
        .register_fn("info", RhaiLogger::info)
        .register_fn("warn", RhaiLogger::warn)
        .register_fn("error", RhaiLogger::error);

    engine
        .register_type::<RhaiScriptResult>()
        .register_fn("header", RhaiScriptResult::header);

    engine.register_fn("http", |status: i64| {
        RhaiScriptResult(ScriptResult::http(clamp_u16(status), None))
    });
    engine.register_fn("http", |status: i64, body: Dynamic| {
        RhaiScriptResult(ScriptResult::http(
            clamp_u16(status),
            Some(dynamic_to_script_result_body(body)),
        ))
    });
    engine.register_fn("delay", |ms: i64| {
        RhaiScriptResult(ScriptResult::Delay(ms.max(0) as u64))
    });
    engine.register_fn("reset", || RhaiScriptResult(ScriptResult::Reset));
    engine.register_fn("pass", || RhaiScriptResult(ScriptResult::Pass));

    // `ctx.request.header("X-Flow-Id")` / `ctx.response.header(...)`: case-insensitive lookup
    // into the receiver map's own `headers` field. Registered generically on `Map` (Rhai method
    // calls dispatch on the receiver's runtime type), so it's a harmless no-op on any other map
    // that doesn't happen to carry a `headers` key.
    engine.register_fn("header", |m: &mut Map, name: &str| -> Dynamic {
        let Some(headers_val) = m.get("headers") else {
            return Dynamic::UNIT;
        };
        let Some(headers_map) = headers_val.clone().try_cast::<Map>() else {
            return Dynamic::UNIT;
        };
        headers_map
            .iter()
            .find(|(k, _)| k.as_str().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or(Dynamic::UNIT)
    });
}

fn clamp_u16(n: i64) -> u16 {
    n.clamp(0, i64::from(u16::MAX)) as u16
}

fn dynamic_to_script_result_body(value: Dynamic) -> ScriptResultBody {
    if let Some(s) = value.clone().try_cast::<String>() {
        ScriptResultBody::Str(s)
    } else {
        ScriptResultBody::Json(dynamic_to_json(value))
    }
}

/// Flow-state handle bound to one flow id — `ctx.state` and the value returned by
/// `ctx.store.flow(id)` (issue #357 Item 1). Exposes the subset of `ScriptFlowStore` that doesn't
/// need a `flow_id` argument per call (full get_or/incr-with-args/cas is P3b, issue #358).
#[derive(Clone)]
pub struct RhaiStateHandle {
    inner: ScriptFlowStore,
    flow_id: String,
}

impl RhaiStateHandle {
    fn new(store: Arc<dyn FlowStore>, flow_id: String) -> Self {
        Self {
            inner: ScriptFlowStore::new(store),
            flow_id,
        }
    }

    pub fn get(&mut self, key: String) -> std::result::Result<Dynamic, Box<rhai::EvalAltResult>> {
        self.inner.get(self.flow_id.clone(), key)
    }

    pub fn set(
        &mut self,
        key: String,
        value: Dynamic,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        self.inner.set(self.flow_id.clone(), key, value)
    }

    pub fn incr(&mut self, key: String) -> std::result::Result<i64, Box<rhai::EvalAltResult>> {
        self.inner.increment(self.flow_id.clone(), key)
    }

    pub fn exists(&mut self, key: String) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        self.inner.exists(self.flow_id.clone(), key)
    }

    pub fn delete(&mut self, key: String) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        self.inner.delete(self.flow_id.clone(), key)
    }
}

/// `ctx.store`: the flow-store escape hatch (issue #357 Item 1) — `.flow(id)` returns a handle
/// scoped to an arbitrary (not necessarily the request's own) flow id.
#[derive(Clone)]
pub struct RhaiStoreHandle {
    store: Arc<dyn FlowStore>,
}

impl RhaiStoreHandle {
    fn new(store: Arc<dyn FlowStore>) -> Self {
        Self { store }
    }

    pub fn flow(&mut self, flow_id: String) -> RhaiStateHandle {
        RhaiStateHandle::new(Arc::clone(&self.store), flow_id)
    }
}

/// `ctx.logger`: real `debug`/`info`/`warn`/`error`, routed to `tracing` at target
/// `"rift::script"` (issue #357 Item 1, reusing P1's logging target — issue #355).
#[derive(Clone)]
pub struct RhaiLogger {
    port: u16,
    stub_id: Option<String>,
}

impl RhaiLogger {
    fn log(&self, level: tracing::Level, message: &str) {
        let port = self.port;
        let stub_id = self.stub_id.as_deref().unwrap_or("");
        match level {
            tracing::Level::DEBUG => {
                tracing::debug!(target: "rift::script", port, stub_id, "{message}")
            }
            tracing::Level::INFO => {
                tracing::info!(target: "rift::script", port, stub_id, "{message}")
            }
            tracing::Level::WARN => {
                tracing::warn!(target: "rift::script", port, stub_id, "{message}")
            }
            _ => tracing::error!(target: "rift::script", port, stub_id, "{message}"),
        }
    }

    pub fn debug(&mut self, message: String) {
        self.log(tracing::Level::DEBUG, &message);
    }
    pub fn info(&mut self, message: String) {
        self.log(tracing::Level::INFO, &message);
    }
    pub fn warn(&mut self, message: String) {
        self.log(tracing::Level::WARN, &message);
    }
    pub fn error(&mut self, message: String) {
        self.log(tracing::Level::ERROR, &message);
    }
}

/// The v2 result-constructor builder (issue #357 Item 3): `http()`/`delay()`/`reset()`/`pass()`
/// all produce one of these; `.header(k, v)` mutates and returns a clone for chaining (Rhai's
/// builder idiom — a `()`-returning method can't be chained).
#[derive(Clone)]
pub struct RhaiScriptResult(pub ScriptResult);

impl RhaiScriptResult {
    pub fn header(&mut self, key: String, value: String) -> Self {
        self.0.add_header(key, value);
        self.clone()
    }
}

/// What a script entrypoint call returned, tagged with which contract (v1 vs v2) produced it —
/// each has its own result parser (issue #357 Item 4).
enum EntrypointOutcome {
    V1(Dynamic),
    V2(Dynamic),
}

/// Run one v2-placement entrypoint (`respond`/`matches`/`transform`/`delay`) or its v1
/// `should_inject` fallback (issue #357 Items 2/4). Detection: `should_inject` defined AND
/// `entrypoint == "respond"` → v1. Else a function named `entrypoint` → call it with `ctx`. Else
/// (bare-expression form) → evaluate the whole script with `ctx` in scope and use the tail
/// expression's value.
fn run_entrypoint(
    engine: &Engine,
    ast: &AST,
    entrypoint: &str,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> Result<EntrypointOutcome> {
    let mut scope = Scope::new();
    let request_map = create_request_map(request);
    let flow_store_wrapper = ScriptFlowStore::new(Arc::clone(&flow_store));
    let ctx_map = build_ctx_map(ctx_input, flow_store);

    let has_should_inject = entrypoint == entrypoints::RESPOND
        && ast
            .iter_functions()
            .any(|f| f.name == entrypoints::SHOULD_INJECT);

    if has_should_inject {
        engine
            .run_ast_with_scope(&mut scope, ast)
            .map_err(|e| anyhow!("Script execution error: {e}"))?;
        let result: Dynamic = engine
            .call_fn(
                &mut scope,
                ast,
                entrypoints::SHOULD_INJECT,
                (request_map, flow_store_wrapper),
            )
            .map_err(|e| anyhow!("Failed to call should_inject function: {e}"))?;
        return Ok(EntrypointOutcome::V1(result));
    }

    let has_named = ast.iter_functions().any(|f| f.name == entrypoint);
    if has_named {
        engine
            .run_ast_with_scope(&mut scope, ast)
            .map_err(|e| anyhow!("Script execution error: {e}"))?;
        let result: Dynamic = engine
            .call_fn(&mut scope, ast, entrypoint, (ctx_map,))
            .map_err(|e| anyhow!("Failed to call {entrypoint}(ctx): {e}"))?;
        Ok(EntrypointOutcome::V2(result))
    } else {
        // Bare-expression script (issue #357 Item 2): the whole body IS the function, with `ctx`
        // in scope; its tail expression is the return value (Rhai's normal "eval" semantics).
        let has_any_function = ast.iter_functions().next().is_some();
        scope.push("ctx", ctx_map);
        let result: Dynamic = engine
            .eval_ast_with_scope(&mut scope, ast)
            .map_err(|e| anyhow!("Script execution error: {e}"))?;
        // B1 (issue #357, "nothing fails silently"): a script that declares function(s) but none
        // is the requested entrypoint (nor `should_inject`) and whose top-level completion value
        // is unit almost certainly has a MISNAMED entrypoint (e.g. `fn respnod(ctx)`). Falling
        // back to the bare-expression path would silently yield `None` — a normal response served
        // with no sign the script never ran. Surface it as an explicit error instead. A genuine
        // bare expression is still fine: it either has no function declarations, or produces a
        // non-unit value.
        if result.is_unit() && has_any_function {
            return Err(anyhow!(
                "script defines function(s) but none is the `{entrypoint}` entrypoint \
                 (and there is no bare expression to evaluate); did you mean `{entrypoint}`?"
            ));
        }
        Ok(EntrypointOutcome::V2(result))
    }
}

fn dynamic_to_fault_decision(result: Dynamic, rule_id: &str) -> Result<FaultDecision> {
    if result.is_unit() {
        return Ok(FaultDecision::None);
    }
    let script_result = result.try_cast::<RhaiScriptResult>().ok_or_else(|| {
        anyhow!("respond(ctx) must return http(...)/delay(...)/reset()/pass() or nothing")
    })?;
    Ok(script_result.0.into_fault_decision(rule_id))
}

fn dynamic_to_matches_bool(result: Dynamic) -> bool {
    result.as_bool().unwrap_or(false)
}

fn dynamic_to_delay_ms(result: Dynamic) -> Result<u64> {
    if let Ok(n) = result.as_int() {
        Ok(n.max(0) as u64)
    } else if let Ok(f) = result.as_float() {
        Ok(f.max(0.0) as u64)
    } else {
        Err(anyhow!("delay(ctx) must return a number of milliseconds"))
    }
}

fn dynamic_to_transform_result(result: Dynamic) -> Result<Option<ScriptResult>> {
    if result.is_unit() {
        return Ok(None);
    }
    let script_result = result
        .try_cast::<RhaiScriptResult>()
        .ok_or_else(|| anyhow!("transform(ctx) must return http(...)/pass() or nothing"))?;
    Ok(Some(script_result.0))
}

/// `respond(ctx)` (issue #357 Item 2): the response-script entrypoint. Auto-detects the v1
/// `should_inject` wrapper (issue #357 Item 4).
pub fn call_respond(
    engine: &Engine,
    ast: &AST,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    match run_entrypoint(
        engine,
        ast,
        entrypoints::RESPOND,
        request,
        ctx_input,
        flow_store,
    )? {
        EntrypointOutcome::V1(d) => parse_fault_decision_with_rule_id(d, rule_id),
        EntrypointOutcome::V2(d) => dynamic_to_fault_decision(d, rule_id),
    }
}

/// `matches(ctx)` (issue #357 Item 2): the predicate-script entrypoint, returns a bool.
pub fn call_matches(
    engine: &Engine,
    ast: &AST,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> Result<bool> {
    match run_entrypoint(
        engine,
        ast,
        entrypoints::MATCHES,
        request,
        ctx_input,
        flow_store,
    )? {
        EntrypointOutcome::V1(_) => Err(anyhow!(
            "matches(ctx) does not support the v1 should_inject wrapper"
        )),
        EntrypointOutcome::V2(d) => Ok(dynamic_to_matches_bool(d)),
    }
}

/// `transform(ctx)` (issue #357 Item 2): the decorate-behavior entrypoint. Returns `None` when
/// the script returns nothing (no change to the response); `Some(result)` when it returns an
/// `http(...)`/`pass()` result constructor describing the new response. NOTE: unlike a true
/// in-place `ctx.response` mutation, this iteration requires the new response to be *returned* —
/// see the module-level doc for why in-place sharing isn't guaranteed across engines yet.
pub fn call_transform(
    engine: &Engine,
    ast: &AST,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> Result<Option<ScriptResult>> {
    match run_entrypoint(
        engine,
        ast,
        entrypoints::TRANSFORM,
        request,
        ctx_input,
        flow_store,
    )? {
        EntrypointOutcome::V1(_) => Err(anyhow!(
            "transform(ctx) does not support the v1 should_inject wrapper"
        )),
        EntrypointOutcome::V2(d) => dynamic_to_transform_result(d),
    }
}

/// `delay(ctx)` (issue #357 Item 2): the wait-behavior entrypoint, returns a millisecond count.
pub fn call_delay(
    engine: &Engine,
    ast: &AST,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> Result<u64> {
    match run_entrypoint(
        engine,
        ast,
        entrypoints::DELAY,
        request,
        ctx_input,
        flow_store,
    )? {
        EntrypointOutcome::V1(_) => Err(anyhow!(
            "delay(ctx) does not support the v1 should_inject wrapper"
        )),
        EntrypointOutcome::V2(d) => dynamic_to_delay_ms(d),
    }
}

fn header_map_lowercased(headers: &std::collections::HashMap<String, String>) -> Map {
    let mut m = Map::new();
    for (k, v) in headers {
        m.insert(k.to_ascii_lowercase().into(), Dynamic::from(v.clone()));
    }
    m
}

/// Parse `raw` as JSON for `ctx.request.json`/`ctx.response.json` (issue #357 Item 1): valid JSON
/// (including the literal `null`) or a parse failure both yield `Dynamic::UNIT` — Rhai's "nothing"
/// — since a `null` body and a non-JSON body are indistinguishable at this field's use sites.
fn parse_json_or_unit(raw: &str) -> Dynamic {
    serde_json::from_str::<Value>(raw)
        .map(json_to_dynamic)
        .unwrap_or(Dynamic::UNIT)
}

fn build_request_ctx_map(request: &ScriptRequest) -> Map {
    let mut m = Map::new();
    m.insert("method".into(), Dynamic::from(request.method.clone()));
    m.insert("path".into(), Dynamic::from(request.path.clone()));

    let mut path_params = Map::new();
    for (k, v) in &request.path_params {
        path_params.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    m.insert("pathParams".into(), Dynamic::from(path_params));

    let mut query = Map::new();
    for (k, v) in &request.query {
        query.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    m.insert("query".into(), Dynamic::from(query));

    m.insert(
        "headers".into(),
        Dynamic::from(header_map_lowercased(&request.headers)),
    );

    // ctx.request.body is always the raw string (issue #357 Item 1); fall back to
    // re-serializing the parsed `body` for callers that only populated that field.
    let raw = request.raw_body.clone().unwrap_or_else(|| {
        if request.body.is_null() {
            String::new()
        } else {
            serde_json::to_string(&request.body).unwrap_or_default()
        }
    });
    m.insert("json".into(), parse_json_or_unit(&raw));
    m.insert("body".into(), Dynamic::from(raw));

    m
}

fn build_response_ctx_map(response: &ScriptResponseContext) -> Map {
    let mut m = Map::new();
    m.insert("status".into(), Dynamic::from(response.status as i64));
    m.insert(
        "headers".into(),
        Dynamic::from(header_map_lowercased(&response.headers)),
    );
    m.insert("json".into(), parse_json_or_unit(&response.body));
    m.insert("body".into(), Dynamic::from(response.body.clone()));
    m
}

fn build_stub_ctx_map(stub: &super::ScriptStubContext) -> Map {
    let mut m = Map::new();
    m.insert(
        "scenarioName".into(),
        stub.scenario_name
            .clone()
            .map_or(Dynamic::UNIT, Dynamic::from),
    );
    m.insert(
        "scenarioState".into(),
        stub.scenario_state
            .clone()
            .map_or(Dynamic::UNIT, Dynamic::from),
    );
    m.insert(
        "id".into(),
        stub.stub_id.clone().map_or(Dynamic::UNIT, Dynamic::from),
    );
    m
}

/// Build the v2 `ctx` map (issue #357 Item 1): identical field names/semantics across engines —
/// see the doc comment on [`ScriptCtxInput`].
fn build_ctx_map(input: &ScriptCtxInput, flow_store: Arc<dyn FlowStore>) -> Map {
    let mut ctx = Map::new();
    ctx.insert(
        "request".into(),
        Dynamic::from(build_request_ctx_map(input.request)),
    );
    if let Some(resp) = &input.response {
        ctx.insert(
            "response".into(),
            Dynamic::from(build_response_ctx_map(resp)),
        );
    }
    ctx.insert("flowId".into(), Dynamic::from(input.flow_id.clone()));
    ctx.insert(
        "stub".into(),
        Dynamic::from(build_stub_ctx_map(&input.stub)),
    );
    ctx.insert(
        "state".into(),
        Dynamic::from(RhaiStateHandle::new(
            Arc::clone(&flow_store),
            input.flow_id.clone(),
        )),
    );
    ctx.insert(
        "store".into(),
        Dynamic::from(RhaiStoreHandle::new(Arc::clone(&flow_store))),
    );
    ctx.insert(
        "logger".into(),
        Dynamic::from(RhaiLogger {
            port: input.port,
            stub_id: input.stub.stub_id.clone(),
        }),
    );
    ctx
}

/// Public function to execute Rhai script with a reusable engine (for script pool). Auto-detects
/// v1/v2 the same way [`RhaiEngine::should_inject_fault`] does (issue #357 Item 4); `ctx` gets
/// best-effort defaults since the pool has no imposter context here.
pub fn execute_rhai_with_engine(
    engine: &Engine,
    ast: &Arc<AST>,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(request);
    call_respond(engine, ast, request, &ctx_input, flow_store, rule_id)
}

/// Helper to parse fault decision with a given rule_id
fn parse_fault_decision_with_rule_id(result: Dynamic, rule_id: &str) -> Result<FaultDecision> {
    if result.is_unit() {
        return Ok(FaultDecision::None);
    }

    let map = result
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("Script must return a map"))?;

    // Check inject flag
    let inject = map
        .get("inject")
        .and_then(|v| v.as_bool().ok())
        .unwrap_or(false);

    if !inject {
        return Ok(FaultDecision::None);
    }

    // Get fault type
    let fault_type = map
        .get("fault")
        .and_then(|v| v.clone().try_cast::<String>())
        .ok_or_else(|| anyhow!("Missing 'fault' field"))?;

    match fault_type.as_str() {
        "latency" => {
            let duration_ms = map
                .get("duration_ms")
                .and_then(|v| v.as_int().ok())
                .ok_or_else(|| anyhow!("Missing 'duration_ms' for latency fault"))?;

            Ok(FaultDecision::Latency {
                duration_ms: duration_ms as u64,
                rule_id: rule_id.to_string(),
            })
        }
        "error" => {
            let status = map
                .get("status")
                .and_then(|v| v.as_int().ok())
                .ok_or_else(|| anyhow!("Missing 'status' for error fault"))?;

            let body = map
                .get("body")
                .map(|v| {
                    if let Some(s) = v.clone().try_cast::<String>() {
                        s
                    } else {
                        match v.clone().try_cast::<Map>() {
                            Some(m) => {
                                // Convert map to JSON string
                                serde_json::to_string(&dynamic_to_json(Dynamic::from(m)))
                                    .unwrap_or_else(|_| "{}".to_string())
                            }
                            _ => {
                                format!("{v}")
                            }
                        }
                    }
                })
                .unwrap_or_else(|| "{}".to_string());

            // Extract optional headers map
            let mut headers = std::collections::HashMap::new();
            if let Some(headers_value) = map.get("headers")
                && let Some(headers_map) = headers_value.clone().try_cast::<Map>()
            {
                for (key, value) in headers_map {
                    // Try to convert value to string
                    let value_str = if let Some(s) = value.clone().try_cast::<String>() {
                        s
                    } else {
                        format!("{value}")
                    };
                    headers.insert(key.to_string(), value_str);
                }
            }

            Ok(FaultDecision::Error {
                status: status as u16,
                body,
                rule_id: rule_id.to_string(),
                headers,
            })
        }
        _ => Err(anyhow!("Unknown fault type: {fault_type}")),
    }
}

// Helper functions to convert between Rhai Dynamic and serde_json::Value

pub(super) fn json_to_dynamic(value: Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        Value::String(s) => Dynamic::from(s),
        Value::Array(arr) => {
            let vec: Vec<Dynamic> = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(vec)
        }
        Value::Object(obj) => {
            let mut map = Map::new();
            for (k, v) in obj {
                map.insert(k.into(), json_to_dynamic(v));
            }
            Dynamic::from(map)
        }
    }
}

pub(super) fn dynamic_to_json(value: Dynamic) -> Value {
    if value.is_unit() {
        Value::Null
    } else if let Ok(b) = value.as_bool() {
        Value::Bool(b)
    } else if let Ok(i) = value.as_int() {
        Value::Number(i.into())
    } else if let Ok(f) = value.as_float() {
        Value::Number(serde_json::Number::from_f64(f).unwrap_or(0.into()))
    } else if let Some(s) = value.clone().try_cast::<String>() {
        Value::String(s)
    } else {
        match value.clone().try_cast::<Vec<Dynamic>>() {
            Some(arr) => Value::Array(arr.into_iter().map(dynamic_to_json).collect()),
            _ => match value.clone().try_cast::<Map>() {
                Some(map) => {
                    let mut obj = serde_json::Map::new();
                    for (k, v) in map {
                        obj.insert(k.to_string(), dynamic_to_json(v));
                    }
                    Value::Object(obj)
                }
                _ => Value::String(format!("{value}")),
            },
        }
    }
}

/// Run a Rhai `should_inject` with a wall-clock interrupt hook (issue #308). While the AST
/// evaluates, Rhai calls the registered `on_progress` callback periodically; when `abort`
/// is set (by the caller's deadline), it returns `Some(_)`, terminating execution with an
/// error — the same mechanism the pooled path uses (#172).
///
/// The AST is compiled through the content-addressed cache (issue #356): repeated requests for
/// the same `code` (e.g. several stubs `ref:`-ing the same registry entry, or repeated requests
/// against the same stub) reuse the compiled AST instead of recompiling every call.
pub fn run_should_inject_with_abort_rhai(
    code: &str,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    abort: &Arc<AtomicBool>,
    ctx_extra: &ScriptCtxExtras,
) -> Result<FaultDecision> {
    let ast = super::compiled_cache::cached_rhai_ast(code)?;
    let mut engine = RhaiEngine::create_engine();
    let flag = Arc::clone(abort);
    engine.on_progress(move |_ops| {
        if flag.load(Ordering::Relaxed) {
            Some(Dynamic::TRUE)
        } else {
            None
        }
    });
    let ctx_input = ctx_extra.build_ctx_input(request);
    call_respond(&engine, &ast, request, &ctx_input, flow_store, rule_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InMemoryFlowStore;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_simple_fault_injection() {
        // New unified pattern: define should_inject(request, flow_store) function
        let script = r#"
            fn should_inject(request, flow_store) {
                if request.method == "POST" {
                    return #{
                        inject: true,
                        fault: "error",
                        status: 503,
                        body: "Service unavailable"
                    };
                }
                #{ inject: false }
            }
        "#;

        let engine = RhaiEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
            raw_body: None,
            method: "POST".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let decision = engine.should_inject_fault(&request, store).unwrap();

        match decision {
            FaultDecision::Error {
                status,
                body,
                rule_id,
                headers,
            } => {
                assert_eq!(status, 503);
                assert_eq!(body, "Service unavailable");
                assert_eq!(rule_id, "test-rule");
                assert!(headers.is_empty()); // No headers in this test
            }
            _ => panic!("Expected Error fault decision"),
        }
    }

    #[tokio::test]
    async fn test_latency_fault() {
        let script = r#"
            fn should_inject(request, flow_store) {
                #{
                    inject: true,
                    fault: "latency",
                    duration_ms: 500
                }
            }
        "#;

        let engine = RhaiEngine::new(script, "latency-rule").unwrap();
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

        let decision = engine.should_inject_fault(&request, store).unwrap();

        match decision {
            FaultDecision::Latency {
                duration_ms,
                rule_id,
            } => {
                assert_eq!(duration_ms, 500);
                assert_eq!(rule_id, "latency-rule");
            }
            _ => panic!("Expected Latency fault decision"),
        }
    }

    #[tokio::test]
    async fn test_flow_store_increment() {
        let script = r#"
            fn should_inject(request, flow_store) {
                let flow_id = request.headers["x-flow-id"];
                let attempts = flow_store.increment(flow_id, "attempts");

                if attempts <= 2 {
                    return #{
                        inject: true,
                        fault: "error",
                        status: 503,
                        body: "Retry later"
                    };
                }

                #{ inject: false }
            }
        "#;

        let engine = RhaiEngine::new(script, "retry-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow123".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: headers.clone(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        // First attempt - should inject
        let decision1 = engine
            .should_inject_fault(&request, Arc::clone(&store))
            .unwrap();
        assert!(matches!(decision1, FaultDecision::Error { .. }));

        // Second attempt - should inject
        let decision2 = engine
            .should_inject_fault(&request, Arc::clone(&store))
            .unwrap();
        assert!(matches!(decision2, FaultDecision::Error { .. }));

        // Third attempt - should NOT inject
        let decision3 = engine.should_inject_fault(&request, store).unwrap();
        assert!(matches!(decision3, FaultDecision::None));
    }

    #[tokio::test]
    async fn test_header_based_routing() {
        let script = r#"
            fn should_inject(request, flow_store) {
                let user_id = request.headers["x-user-id"];

                if user_id.starts_with("beta-") {
                    return #{
                        inject: true,
                        fault: "latency",
                        duration_ms: 1000
                    };
                }

                #{ inject: false }
            }
        "#;

        let engine = RhaiEngine::new(script, "beta-users").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Beta user - should inject
        let mut headers1 = HashMap::new();
        headers1.insert("x-user-id".to_string(), "beta-user-123".to_string());

        let request1 = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: headers1,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let decision1 = engine
            .should_inject_fault(&request1, Arc::clone(&store))
            .unwrap();
        assert!(matches!(decision1, FaultDecision::Latency { .. }));

        // Regular user - should NOT inject
        let mut headers2 = HashMap::new();
        headers2.insert("x-user-id".to_string(), "regular-user-456".to_string());

        let request2 = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: headers2,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let decision2 = engine.should_inject_fault(&request2, store).unwrap();
        assert!(matches!(decision2, FaultDecision::None));
    }

    #[tokio::test]
    async fn test_ast_caching_with_reusable_engine() {
        // This test verifies that AST is wrapped in Arc and can be reused
        // across multiple executions with a reusable engine (Day 3 feature)
        let script = r#"
            fn should_inject(request, flow_store) {
                if request.path == "/cache-test" {
                    return #{
                        inject: true,
                        fault: "error",
                        status: 429,
                        body: "Rate limited"
                    };
                }
                #{ inject: false }
            }
        "#;

        let engine = RhaiEngine::new(script, "cache-test").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Get AST reference (Arc) - this is what script pool will use
        let ast = engine.ast();

        // Create a reusable engine (simulating script pool worker)
        let reusable_engine = RhaiEngine::create_engine();

        // Execute same AST multiple times with reusable engine
        for i in 0..10 {
            let request = ScriptRequest {
                raw_body: None,
                method: "GET".to_string(),
                path: "/cache-test".to_string(),
                headers: HashMap::new(),
                body: json!({}),
                query: HashMap::new(),
                path_params: HashMap::new(),
            };

            let decision = execute_rhai_with_engine(
                &reusable_engine,
                ast,
                &request,
                Arc::clone(&store),
                "cache-test",
            )
            .unwrap();

            match decision {
                FaultDecision::Error { status, .. } => {
                    assert_eq!(status, 429, "Iteration {i}");
                }
                _ => panic!("Expected Error fault decision on iteration {i}"),
            }
        }

        // Verify AST is actually Arc (cheap clone)
        let ast_clone = engine.ast().clone();
        assert!(
            Arc::ptr_eq(ast, &ast_clone),
            "AST should be same Arc instance"
        );
    }

    // ============================================
    // Issue #357: unified ctx, v2 entrypoints, result constructors
    // ============================================
    mod v2 {
        use super::*;
        use crate::backends::InMemoryFlowStore;

        fn req(headers: HashMap<String, String>, raw_body: Option<&str>) -> ScriptRequest {
            ScriptRequest {
                method: "POST".to_string(),
                path: "/api/orders".to_string(),
                headers,
                body: serde_json::Value::Null,
                query: HashMap::from([("page".to_string(), "2".to_string())]),
                path_params: HashMap::from([("id".to_string(), "42".to_string())]),
                raw_body: raw_body.map(|s| s.to_string()),
            }
        }

        fn store() -> Arc<dyn FlowStore> {
            Arc::new(InMemoryFlowStore::new(300))
        }

        fn run_respond(script: &str, request: &ScriptRequest) -> Result<FaultDecision> {
            let engine = RhaiEngine::new(script, "v2-rule").unwrap();
            engine.should_inject_fault(request, store())
        }

        // --- ctx.request ---

        #[test]
        fn ctx_request_header_is_case_insensitive() {
            let headers = HashMap::from([("X-Flow-Id".to_string(), "flow-9".to_string())]);
            let script = r#"
                fn respond(ctx) {
                    let v = ctx.request.header("x-flow-id");
                    http(200, #{ seen: v })
                }
            "#;
            let decision = run_respond(script, &req(headers, None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => assert!(body.contains("flow-9")),
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_headers_map_is_lowercased() {
            let headers = HashMap::from([("X-Flow-Id".to_string(), "flow-9".to_string())]);
            let script = r#"
                fn respond(ctx) {
                    if ctx.request.headers.contains("x-flow-id") {
                        http(200)
                    } else {
                        http(500)
                    }
                }
            "#;
            let decision = run_respond(script, &req(headers, None)).unwrap();
            match decision {
                FaultDecision::Error { status, .. } => assert_eq!(status, 200),
                other => panic!("expected 200, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_json_lazy_parse_valid() {
            let script = r#"
                fn respond(ctx) {
                    http(200, #{ n: ctx.request.json.n })
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), Some(r#"{"n": 7}"#))).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => assert!(body.contains('7')),
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_json_is_unit_for_non_json_body() {
            let script = r#"
                fn respond(ctx) {
                    if ctx.request.json == () {
                        http(200)
                    } else {
                        http(500)
                    }
                }
            "#;
            let decision =
                run_respond(script, &req(HashMap::new(), Some("not json at all"))).unwrap();
            match decision {
                FaultDecision::Error { status, .. } => assert_eq!(status, 200),
                other => panic!("expected 200 (json is unit), got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_path_params_and_query() {
            let script = r#"
                fn respond(ctx) {
                    http(200, #{ id: ctx.request.pathParams.id, page: ctx.request.query.page })
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("42"));
                    assert!(body.contains('2'));
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_body_is_raw_string() {
            let script = r#"
                fn respond(ctx) {
                    http(200, ctx.request.body)
                }
            "#;
            let decision =
                run_respond(script, &req(HashMap::new(), Some("plain text, not json"))).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => assert_eq!(body, "plain text, not json"),
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        // --- entrypoints: named wrapper + bare, respond/matches/transform/delay ---

        #[test]
        fn respond_named_wrapper() {
            let script = r#"
                fn respond(ctx) {
                    http(503, #{ error: "unavailable" })
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
            // No `fn respond` wrapper at all — the whole script body is the function.
            let script = r#"
                if ctx.request.method == "POST" {
                    http(503, #{ error: "unavailable" })
                } else {
                    pass()
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        #[test]
        fn matches_named_wrapper_true_and_false() {
            let engine = RhaiEngine::create_engine();
            let script = r#" fn matches(ctx) { ctx.request.method == "POST" } "#;
            let ast = engine.compile(script).unwrap();

            let post_req = req(HashMap::new(), None);
            let ctx_input = ScriptCtxInput::new(&post_req, "flow-1");
            let matched =
                call_matches(&engine, &ast, ctx_input.request, &ctx_input, store()).unwrap();
            assert!(matched);

            let mut get_req = req(HashMap::new(), None);
            get_req.method = "GET".to_string();
            let ctx_input2 = ScriptCtxInput::new(&get_req, "flow-1");
            let matched2 =
                call_matches(&engine, &ast, ctx_input2.request, &ctx_input2, store()).unwrap();
            assert!(!matched2);
        }

        #[test]
        fn matches_bare_expression() {
            let engine = RhaiEngine::create_engine();
            let script = r#" ctx.request.path == "/api/orders" "#;
            let ast = engine.compile(script).unwrap();
            let request = req(HashMap::new(), None);
            let ctx_input = ScriptCtxInput::new(&request, "flow-1");
            let matched =
                call_matches(&engine, &ast, ctx_input.request, &ctx_input, store()).unwrap();
            assert!(matched);
        }

        #[test]
        fn transform_named_wrapper_returns_new_response() {
            let engine = RhaiEngine::create_engine();
            let script = r#"
                fn transform(ctx) {
                    http(ctx.response.status, #{ wrapped: ctx.response.body })
                }
            "#;
            let ast = engine.compile(script).unwrap();
            let request = req(HashMap::new(), None);
            let ctx_input =
                ScriptCtxInput::new(&request, "flow-1").with_response(ScriptResponseContext {
                    status: 201,
                    headers: HashMap::new(),
                    body: "original".to_string(),
                });
            let result = call_transform(&engine, &ast, ctx_input.request, &ctx_input, store())
                .unwrap()
                .expect("transform must return a result");
            match result.into_fault_decision("rule") {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 201);
                    assert!(body.contains("original"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn transform_bare_returns_nothing_means_no_change() {
            let engine = RhaiEngine::create_engine();
            let script = "()"; // explicit no-op bare expression
            let ast = engine.compile(script).unwrap();
            let request = req(HashMap::new(), None);
            let ctx_input = ScriptCtxInput::new(&request, "flow-1");
            let result =
                call_transform(&engine, &ast, ctx_input.request, &ctx_input, store()).unwrap();
            assert!(result.is_none());
        }

        #[test]
        fn delay_named_wrapper_and_bare() {
            let engine = RhaiEngine::create_engine();
            let script = " fn delay(ctx) { 250 } ";
            let ast = engine.compile(script).unwrap();
            let request = req(HashMap::new(), None);
            let ctx_input = ScriptCtxInput::new(&request, "flow-1");
            let ms = call_delay(&engine, &ast, ctx_input.request, &ctx_input, store()).unwrap();
            assert_eq!(ms, 250);

            let bare_script = " 100 + 25 ";
            let bare_ast = engine.compile(bare_script).unwrap();
            let ms2 =
                call_delay(&engine, &bare_ast, ctx_input.request, &ctx_input, store()).unwrap();
            assert_eq!(ms2, 125);
        }

        // --- result constructors ---

        #[test]
        fn http_json_body_sets_content_type_unless_overridden() {
            let script = r#"
                fn respond(ctx) {
                    http(503, #{ error: "x" }).header("Retry-After", "1")
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
                    assert_eq!(body, r#"{"error":"x"}"#);
                    assert_eq!(headers.get("Retry-After").map(String::as_str), Some("1"));
                    assert_eq!(
                        headers.get("Content-Type").map(String::as_str),
                        Some("application/json")
                    );
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn http_explicit_content_type_wins() {
            let script = r#"
                fn respond(ctx) {
                    http(200, #{ a: 1 }).header("Content-Type", "application/vnd.custom+json")
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { headers, .. } => {
                    assert_eq!(
                        headers.get("Content-Type").map(String::as_str),
                        Some("application/vnd.custom+json")
                    );
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn http_string_body_passes_through_verbatim() {
            let script = r#"
                fn respond(ctx) {
                    http(200, "hello world")
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, headers, .. } => {
                    assert_eq!(body, "hello world");
                    assert!(!headers.contains_key("Content-Type"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn delay_constructor() {
            let script = " fn respond(ctx) { delay(42) } ";
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 42),
                other => panic!("expected Latency, got {other:?}"),
            }
        }

        #[test]
        fn reset_constructor() {
            let script = " fn respond(ctx) { reset() } ";
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Reset { .. }));
        }

        #[test]
        fn pass_constructor_and_bare_nothing_both_mean_none() {
            let decision =
                run_respond(" fn respond(ctx) { pass() } ", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::None));

            let decision2 =
                run_respond(" fn respond(ctx) { () } ", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision2, FaultDecision::None));
        }

        // --- v1 back-compat ---

        #[test]
        fn v1_should_inject_still_wins_over_v2_detection() {
            let script = r#"
                fn should_inject(request, flow_store) {
                    #{ inject: true, fault: "error", status: 418, body: "teapot" }
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 418);
                    assert_eq!(body, "teapot");
                }
                other => panic!("expected v1 Error decision, got {other:?}"),
            }
        }

        // --- retry example from the issue: ctx.state.incr + http() end-to-end ---

        #[test]
        fn retry_example_end_to_end_with_ctx_state_incr() {
            let script = r#"
                fn respond(ctx) {
                    let n = ctx.state.incr("attempts");
                    if n <= 2 {
                        http(503, #{ error: "unavailable", attempt: n }).header("Retry-After", "1")
                    } else {
                        http(200, #{ ok: true, succeededOnAttempt: n })
                    }
                }
            "#;
            let engine = RhaiEngine::new(script, "retry-rule").unwrap();
            let shared_store = store();
            let request = req(HashMap::new(), None);

            let d1 = engine
                .should_inject_fault(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d1, FaultDecision::Error { status: 503, .. }));

            let d2 = engine
                .should_inject_fault(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d2, FaultDecision::Error { status: 503, .. }));

            let d3 = engine.should_inject_fault(&request, shared_store).unwrap();
            match d3 {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("succeededOnAttempt"));
                }
                other => panic!("expected 200 on third attempt, got {other:?}"),
            }
        }

        // --- ctx.store escape hatch ---

        #[test]
        fn ctx_store_flow_scopes_to_another_flow_id() {
            let script = r#"
                fn respond(ctx) {
                    ctx.store.flow("other-flow").set("shared", 99);
                    let v = ctx.store.flow("other-flow").get("shared");
                    http(200, #{ v: v })
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("99"));
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // --- ctx.stub ---

        #[test]
        fn ctx_stub_none_when_unavailable() {
            let script = r#"
                fn respond(ctx) {
                    if ctx.stub.scenarioName == () && ctx.stub.id == () {
                        http(200)
                    } else {
                        http(500)
                    }
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 200, .. }));
        }

        // B1 (issue #357, "nothing fails silently"): a script that defines ONLY a misnamed
        // entrypoint (`respnod` typo) must Err, not silently fall back to bare → None.
        #[test]
        fn misnamed_entrypoint_errors_not_none() {
            let script = r#"
                fn respnod(ctx) {
                    http(500)
                }
            "#;
            let result = run_respond(script, &req(HashMap::new(), None));
            assert!(
                result.is_err(),
                "a misnamed entrypoint must surface an error, got {result:?}"
            );
        }

        // A genuine bare expression (no function declarations, non-unit value) still works.
        #[test]
        fn genuine_bare_expression_still_ok_after_b1() {
            let decision = run_respond("http(503)", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        // A helper function plus a bare expression producing a value is still allowed (the bare
        // value is non-unit, so B1's "declared-but-unmatched + unit" guard doesn't trip).
        #[test]
        fn helper_function_plus_bare_expression_ok() {
            let script = r#"
                fn helper() { 503 }
                http(helper())
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }
    }
}
