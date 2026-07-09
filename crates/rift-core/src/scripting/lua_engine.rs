use crate::extensions::flow_state::{CasOutcome, FlowStore, flow_result, strict_flow_store};
use crate::scripting::{
    FaultDecision, ScriptCtxExtras, ScriptCtxInput, ScriptRequest, ScriptResponseContext,
    ScriptResult, ScriptResultBody, ScriptStubContext, entrypoints,
};
use anyhow::{Result, anyhow};
use mlua::prelude::*;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Lua script engine for fault injection
///
/// # Script Interface
///
/// Scripts must define a `should_inject` function with the following signature:
///
/// ```lua
/// function should_inject(request, flow_store)
///     -- Your logic here
///     return { inject = false }
/// end
/// ```
///
/// ## Request Object
///
/// The `request` parameter is a table containing:
/// - `method` - HTTP method (string): "GET", "POST", "PUT", "DELETE", etc.
/// - `path` - Request path (string): "/api/users/123"
/// - `headers` - Table of header name (string) to value (string)
/// - `body` - Request body (parsed JSON value or nil)
/// - `query` - Table of query parameter name (string) to value (string)
/// - `pathParams` - Table of path parameters extracted from route patterns
///
/// ## Flow Store Object
///
/// The `flow_store` parameter provides state management across requests:
/// - `flow_store:get(flow_id, key)` - Get a stored value (returns nil if not found)
/// - `flow_store:set(flow_id, key, value)` - Store a value (returns bool)
/// - `flow_store:exists(flow_id, key)` - Check if key exists (returns bool)
/// - `flow_store:delete(flow_id, key)` - Delete a key (returns bool)
/// - `flow_store:increment(flow_id, key)` - Increment counter (returns number)
/// - `flow_store:set_ttl(flow_id, ttl_seconds)` - Set flow expiration (returns bool)
/// - `flow_store:last_error()` - Take the last flow-store op's error (returns nil if none)
///
/// ## Return Value
///
/// The function must return a table with the fault decision:
///
/// ```lua
/// -- No fault injection
/// { inject = false }
///
/// -- Latency injection
/// { inject = true, fault = "latency", duration_ms = 500 }
///
/// -- Error injection
/// { inject = true, fault = "error", status = 503, body = "Service unavailable" }
///
/// -- Error with custom headers
/// {
///     inject = true,
///     fault = "error",
///     status = 429,
///     body = "Rate limited",
///     headers = { ["Retry-After"] = "60" }
/// }
/// ```
///
/// ## Example
///
/// ```lua
/// function should_inject(request, flow_store)
///     -- Rate limit based on flow ID from header
///     local flow_id = request.headers["x-flow-id"]
///     if flow_id then
///         local attempts = flow_store:increment(flow_id, "attempts")
///         if attempts > 3 then
///             return { inject = true, fault = "error", status = 429, body = "Rate limited" }
///         end
///     end
///
///     -- Inject fault for POST requests to specific path
///     if request.method == "POST" and request.path == "/api/test" then
///         return { inject = true, fault = "latency", duration_ms = 100 }
///     end
///
///     return { inject = false }
/// end
/// ```
#[derive(Debug, Clone)]
pub struct LuaEngine {
    script: String,
    rule_id: String,
}

impl LuaEngine {
    pub fn new(script: &str, rule_id: &str) -> Result<Self> {
        // Validate the script COMPILES — not that it runs (`.into_function()` compiles without
        // calling). Issue #357 Item 2 legalizes bare-expression scripts, which reference a
        // top-level `ctx` global that only exists at real execution time (`request`/`flow_store`/
        // `ctx` all bound); `.exec()`-ing an unbound bare script here would spuriously fail with
        // "ctx is nil", and for a wrapper-form script would run its top-level side effects at
        // construction time, not request time. A missing `should_inject` is also no longer a
        // construction error (issue #357 Item 4), matching `RhaiEngine::new`, which never required
        // it either.
        let lua = Lua::new();
        lua.load(script)
            .into_function()
            .map_err(|e| anyhow!("Failed to compile Lua script: {e}"))?;

        Ok(Self {
            script: script.to_string(),
            rule_id: rule_id.to_string(),
        })
    }

    /// Execute the script and determine if a fault should be injected. Auto-detects v1
    /// (`should_inject(request, flow_store)`) vs v2 (`respond(ctx)` or bare) — see
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
        // DEPRECATED: This method spawns a thread+runtime per execution (expensive!)
        // Script pool workers should use execute_lua_with_state() instead
        // Kept for backward compatibility only
        let script = self.script.clone();
        let request = request.clone();
        let rule_id = self.rule_id.clone();
        let extra_owned = extra.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            // Create a new runtime for this thread to handle async FlowStore calls
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            let _guard = rt.enter();

            let ctx_input = extra_owned.build_ctx_input(&request);
            let result =
                Self::execute_respond_in_thread(script, &request, flow_store, rule_id, &ctx_input);
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|_| anyhow!("Lua execution thread panicked"))?
    }

    /// Compile-and-run a script for `should_inject_with_ctx` — replaces the old duplicated
    /// table-building (now shared via [`run_entrypoint_lua`]) with the v1/v2-aware dispatch.
    fn execute_respond_in_thread(
        script: String,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        rule_id: String,
        ctx_input: &ScriptCtxInput,
    ) -> Result<FaultDecision> {
        let lua = Lua::new();
        let chunk_fn = lua
            .load(&script)
            .into_function()
            .map_err(|e| anyhow!("Failed to compile script: {e}"))?;
        call_respond_lua(&lua, &chunk_fn, request, ctx_input, flow_store, &rule_id)
    }
}

/// Compile Lua script to bytecode for efficient execution
/// Returns bytecode that can be loaded and executed by any Lua instance
pub fn compile_to_bytecode(script: &str) -> Result<Vec<u8>> {
    let lua = Lua::new();

    // Compile script to bytecode
    // strip = false to keep debug info for better error messages
    let bytecode = lua
        .load(script)
        .into_function()
        .map_err(|e| anyhow!("Failed to compile Lua script: {e}"))?
        .dump(false); // dump() returns Vec<u8> directly, not Result

    Ok(bytecode)
}

/// Public function to execute Lua bytecode with a reusable Lua state (for script pool). Routes
/// through the shared v1/v2-aware [`call_respond_lua`] (issue #357 Item 4); `ctx` gets
/// best-effort defaults since the pool has no imposter context here.
pub fn execute_lua_bytecode(
    lua: &Lua,
    bytecode: &[u8],
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let chunk_fn = lua
        .load(bytecode)
        .into_function()
        .map_err(|e| anyhow!("Failed to load bytecode: {e}"))?;
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(request);
    call_respond_lua(lua, &chunk_fn, request, &ctx_input, flow_store, rule_id)
}

/// Public function to execute Lua script with a reusable Lua state (for script pool). As
/// [`execute_lua_bytecode`], but compiling from source each call.
pub fn execute_lua_with_state(
    lua: &Lua,
    script: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let chunk_fn = lua
        .load(script)
        .into_function()
        .map_err(|e| anyhow!("Failed to compile script: {e}"))?;
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(request);
    call_respond_lua(lua, &chunk_fn, request, &ctx_input, flow_store, rule_id)
}

/// Helper to parse Lua fault decision with given rule_id
fn parse_fault_decision_lua(_lua: &Lua, result: LuaTable, rule_id: &str) -> Result<FaultDecision> {
    let inject: bool = result.get("inject").unwrap_or(false);

    if !inject {
        return Ok(FaultDecision::None);
    }

    let fault_type: String = result.get("fault").unwrap_or_else(|_| "none".to_string());

    match fault_type.as_str() {
        "latency" => {
            let duration_ms: u64 = result
                .get("duration_ms")
                .map_err(|_| anyhow!("latency fault requires duration_ms field"))?;
            Ok(FaultDecision::Latency {
                duration_ms,
                rule_id: rule_id.to_string(),
            })
        }
        "error" => {
            let status: u16 = result
                .get("status")
                .map_err(|_| anyhow!("error fault requires status field"))?;
            let body: String = result.get("body").unwrap_or_else(|_| "".to_string());

            // Extract optional headers table
            let mut headers = std::collections::HashMap::new();
            if let Ok(headers_table) = result.get::<LuaTable>("headers") {
                for (key, value) in headers_table.pairs::<LuaValue, LuaValue>().flatten() {
                    // Convert key to string
                    let key_str = match key {
                        LuaValue::String(s) => {
                            s.to_str().map(|s| s.to_string()).unwrap_or_default()
                        }
                        LuaValue::Integer(i) => i.to_string(),
                        LuaValue::Number(n) => n.to_string(),
                        _ => continue, // Skip non-stringable keys
                    };

                    // Convert value to string
                    let value_str = match value {
                        LuaValue::String(s) => {
                            s.to_str().map(|s| s.to_string()).unwrap_or_default()
                        }
                        LuaValue::Integer(i) => i.to_string(),
                        LuaValue::Number(n) => n.to_string(),
                        LuaValue::Boolean(b) => b.to_string(),
                        _ => continue, // Skip non-stringable values
                    };

                    headers.insert(key_str, value_str);
                }
            }

            Ok(FaultDecision::Error {
                status,
                body,
                rule_id: rule_id.to_string(),
                headers,
            })
        }
        _ => Ok(FaultDecision::None),
    }
}

// =============================================================================
// v2 `ctx` API (issue #357 Items 1-4)
// =============================================================================

/// What a script entrypoint call returned, tagged with which contract (v1 vs v2) produced it.
enum LuaEntrypointOutcome {
    V1(LuaValue),
    V2(LuaValue),
}

fn build_v1_request_table(lua: &Lua, request: &ScriptRequest) -> LuaResult<LuaTable> {
    let request_table = lua.create_table()?;
    request_table.set("method", request.method.clone())?;
    request_table.set("path", request.path.clone())?;

    let headers_table = lua.create_table()?;
    for (k, v) in &request.headers {
        headers_table.set(k.as_str(), v.as_str())?;
    }
    request_table.set("headers", headers_table)?;

    let body_value = json_to_lua(lua, &request.body)?;
    request_table.set("body", body_value)?;

    let query_table = lua.create_table()?;
    for (k, v) in &request.query {
        query_table.set(k.as_str(), v.as_str())?;
    }
    request_table.set("query", query_table)?;

    let path_params_table = lua.create_table()?;
    for (k, v) in &request.path_params {
        path_params_table.set(k.as_str(), v.as_str())?;
    }
    request_table.set("pathParams", path_params_table)?;

    Ok(request_table)
}

fn parse_json_or_nil(lua: &Lua, raw: &str) -> LuaResult<LuaValue> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => json_to_lua(lua, &v),
        Err(_) => Ok(LuaValue::Nil),
    }
}

/// Case-insensitive `header(name)` getter closure shared by `ctx.request`/`ctx.response`. Accepts
/// both `t:header("X")` (colon self-call — the idiomatic Lua form) and `t.header("X")`.
fn make_header_getter(
    lua: &Lua,
    headers: &std::collections::HashMap<String, String>,
) -> LuaResult<LuaFunction> {
    let owned: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    lua.create_function(move |lua, args: LuaMultiValue| -> LuaResult<LuaValue> {
        let name = args
            .iter()
            .rev()
            .find_map(|v| v.as_str().map(|s| s.to_string()));
        match name.and_then(|n| {
            owned
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&n))
                .map(|(_, v)| v.clone())
        }) {
            Some(v) => Ok(LuaValue::String(lua.create_string(&v)?)),
            None => Ok(LuaValue::Nil),
        }
    })
}

fn build_request_ctx_table(lua: &Lua, request: &ScriptRequest) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("method", request.method.clone())?;
    t.set("path", request.path.clone())?;

    let path_params = lua.create_table()?;
    for (k, v) in &request.path_params {
        path_params.set(k.as_str(), v.as_str())?;
    }
    t.set("pathParams", path_params)?;

    let query = lua.create_table()?;
    for (k, v) in &request.query {
        query.set(k.as_str(), v.as_str())?;
    }
    t.set("query", query)?;

    let mut lowercased = std::collections::HashMap::new();
    for (k, v) in &request.headers {
        lowercased.insert(k.to_ascii_lowercase(), v.clone());
    }
    let headers_table = lua.create_table()?;
    for (k, v) in &lowercased {
        headers_table.set(k.as_str(), v.as_str())?;
    }
    t.set("headers", headers_table)?;
    t.set("header", make_header_getter(lua, &request.headers)?)?;

    // ctx.request.body is always the raw string (issue #357 Item 1); fall back to
    // re-serializing the parsed `body` for callers that only populated that field.
    let raw = request.raw_body.clone().unwrap_or_else(|| {
        if request.body.is_null() {
            String::new()
        } else {
            serde_json::to_string(&request.body).unwrap_or_default()
        }
    });
    t.set("json", parse_json_or_nil(lua, &raw)?)?;
    t.set("body", raw)?;

    Ok(t)
}

fn build_response_ctx_table(lua: &Lua, response: &ScriptResponseContext) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("status", response.status)?;
    let headers_table = lua.create_table()?;
    for (k, v) in &response.headers {
        headers_table.set(k.to_ascii_lowercase(), v.as_str())?;
    }
    t.set("headers", headers_table)?;
    t.set("header", make_header_getter(lua, &response.headers)?)?;
    t.set("json", parse_json_or_nil(lua, &response.body)?)?;
    t.set("body", response.body.clone())?;
    Ok(t)
}

fn build_stub_ctx_table(lua: &Lua, stub: &ScriptStubContext) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("scenarioName", stub.scenario_name.clone())?;
    t.set("scenarioState", stub.scenario_state.clone())?;
    t.set("id", stub.stub_id.clone())?;
    Ok(t)
}

/// Flow-state handle bound to one flow id — `ctx.state` and the value returned by
/// `ctx.store:flow(id)` (issue #357 Item 1); atomic ops `get_or`/`incr_by`/`cas`/`ttl` added by
/// issue #358.
struct LuaStateHandle {
    inner: LuaFlowStore,
    flow_id: String,
}

impl LuaStateHandle {
    fn new(store: Arc<dyn FlowStore>, flow_id: String) -> Self {
        Self {
            // v2 `ctx.state` is always fail-loud (issue #358), independent of the env toggle that
            // gates the legacy v1 `flow_store` global.
            inner: LuaFlowStore::new_strict(store),
            flow_id,
        }
    }
}

impl LuaUserData for LuaStateHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, key: String| {
            this.inner.get(lua, this.flow_id.clone(), key)
        });
        methods.add_method("set", |lua, this, (key, value): (String, LuaValue)| {
            this.inner.set(lua, this.flow_id.clone(), key, value)
        });
        methods.add_method("incr", |_lua, this, key: String| {
            this.inner.increment(this.flow_id.clone(), key)
        });
        methods.add_method("exists", |_lua, this, key: String| {
            this.inner.exists(this.flow_id.clone(), key)
        });
        methods.add_method("delete", |_lua, this, key: String| {
            this.inner.delete(this.flow_id.clone(), key)
        });
        // Atomic ops + ergonomic getters (issue #358).
        methods.add_method("get_or", |lua, this, (key, default): (String, LuaValue)| {
            this.inner.get_or(lua, this.flow_id.clone(), key, default)
        });
        methods.add_method("incr_by", |_lua, this, (key, by): (String, i64)| {
            this.inner.increment_by(this.flow_id.clone(), key, by)
        });
        methods.add_method(
            "cas",
            |lua, this, (key, expected, new): (String, LuaValue, LuaValue)| {
                this.inner
                    .cas(lua, this.flow_id.clone(), key, expected, new)
            },
        );
        methods.add_method("ttl", |_lua, this, seconds: i64| {
            this.inner.ttl(this.flow_id.clone(), seconds)
        });
    }
}

/// `ctx.store`: the flow-store escape hatch (issue #357 Item 1) — `:flow(id)` returns a handle
/// scoped to an arbitrary flow id.
struct LuaStoreHandle {
    store: Arc<dyn FlowStore>,
}

impl LuaUserData for LuaStoreHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("flow", |_lua, this, flow_id: String| {
            Ok(LuaStateHandle::new(Arc::clone(&this.store), flow_id))
        });
    }
}

/// `ctx.logger`: real `debug`/`info`/`warn`/`error`, routed to `tracing` at target
/// `"rift::script"` (issue #357 Item 1, reusing P1's logging target — issue #355).
struct LuaLoggerHandle {
    port: u16,
    stub_id: Option<String>,
}

impl LuaLoggerHandle {
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
}

impl LuaUserData for LuaLoggerHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("debug", |_lua, this, message: String| {
            this.log(tracing::Level::DEBUG, &message);
            Ok(())
        });
        methods.add_method("info", |_lua, this, message: String| {
            this.log(tracing::Level::INFO, &message);
            Ok(())
        });
        methods.add_method("warn", |_lua, this, message: String| {
            this.log(tracing::Level::WARN, &message);
            Ok(())
        });
        methods.add_method("error", |_lua, this, message: String| {
            this.log(tracing::Level::ERROR, &message);
            Ok(())
        });
    }
}

/// The v2 result-constructor builder (issue #357 Item 3): `http()`/`delay()`/`reset()`/`pass()`
/// all produce one of these; `:header(k, v)` mutates and returns a clone for chaining.
#[derive(Clone)]
struct LuaScriptResultHandle(ScriptResult);

impl LuaUserData for LuaScriptResultHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("header", |_lua, this, (k, v): (String, String)| {
            this.0.add_header(k, v);
            Ok(this.clone())
        });
    }
}

fn clamp_u16(n: i64) -> u16 {
    n.clamp(0, i64::from(u16::MAX)) as u16
}

fn lua_value_to_script_result_body(lua: &Lua, value: LuaValue) -> LuaResult<ScriptResultBody> {
    match &value {
        LuaValue::String(s) => Ok(ScriptResultBody::Str(s.to_str()?.to_string())),
        _ => Ok(ScriptResultBody::Json(lua_to_json(lua, value)?)),
    }
}

/// Register the v2 result constructors (issue #357 Item 3) as globals on `lua`. Cheap and
/// idempotent, so it's called on every entrypoint run rather than requiring every `Lua::new()`
/// call site in this module to remember to register them once.
fn register_result_constructors(lua: &Lua) -> LuaResult<()> {
    let globals = lua.globals();

    let http_fn = lua.create_function(
        |lua, args: LuaMultiValue| -> LuaResult<LuaScriptResultHandle> {
            let mut it = args.into_iter();
            let status = it.next().and_then(|v| v.as_i64()).unwrap_or(200);
            let body = match it.next() {
                None | Some(LuaValue::Nil) => None,
                Some(v) => Some(lua_value_to_script_result_body(lua, v)?),
            };
            Ok(LuaScriptResultHandle(ScriptResult::http(
                clamp_u16(status),
                body,
            )))
        },
    )?;
    globals.set("http", http_fn)?;

    let delay_fn = lua.create_function(|_lua, ms: i64| {
        Ok(LuaScriptResultHandle(ScriptResult::Delay(ms.max(0) as u64)))
    })?;
    globals.set("delay", delay_fn)?;

    let reset_fn =
        lua.create_function(|_lua, ()| Ok(LuaScriptResultHandle(ScriptResult::Reset)))?;
    globals.set("reset", reset_fn)?;

    let pass_fn = lua.create_function(|_lua, ()| Ok(LuaScriptResultHandle(ScriptResult::Pass)))?;
    globals.set("pass", pass_fn)?;

    Ok(())
}

fn lua_value_to_fault_decision(value: LuaValue, rule_id: &str) -> Result<FaultDecision> {
    match value {
        LuaValue::Nil => Ok(FaultDecision::None),
        LuaValue::UserData(ud) => {
            let handle = ud
                .borrow::<LuaScriptResultHandle>()
                .map_err(|e| anyhow!("respond(ctx) result error: {e}"))?;
            Ok(handle.0.clone().into_fault_decision(rule_id))
        }
        _ => Err(anyhow!(
            "respond(ctx) must return http(...)/delay(...)/reset()/pass() or nothing"
        )),
    }
}

/// Build the v2 `ctx` table (issue #357 Item 1): identical field names/semantics across engines —
/// see the doc comment on [`ScriptCtxInput`].
fn build_ctx_table(
    lua: &Lua,
    input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> LuaResult<LuaTable> {
    let ctx = lua.create_table()?;
    ctx.set("request", build_request_ctx_table(lua, input.request)?)?;
    if let Some(resp) = &input.response {
        ctx.set("response", build_response_ctx_table(lua, resp)?)?;
    }
    ctx.set("flowId", input.flow_id.clone())?;
    ctx.set("stub", build_stub_ctx_table(lua, &input.stub)?)?;
    ctx.set(
        "state",
        LuaStateHandle::new(Arc::clone(&flow_store), input.flow_id.clone()),
    )?;
    ctx.set(
        "store",
        LuaStoreHandle {
            store: Arc::clone(&flow_store),
        },
    )?;
    ctx.set(
        "logger",
        LuaLoggerHandle {
            port: input.port,
            stub_id: input.stub.stub_id.clone(),
        },
    )?;
    Ok(ctx)
}

/// Run one v2-placement entrypoint or its v1 `should_inject` fallback (issue #357 Items 2/4).
/// Detection: the loaded chunk is called once (globals `request`/`flow_store`/`ctx` already set,
/// so both forms have what they need); its return value is the bare-expression result. Then: a
/// global `should_inject` AND `entrypoint == "respond"` → v1 (call it with the v1 args). Else a
/// global function named `entrypoint` → v2 named (call it with `ctx`). Else → v2 bare (the
/// chunk's own return value from the single call above).
fn run_entrypoint_lua(
    lua: &Lua,
    chunk_fn: &LuaFunction,
    entrypoint: &str,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
) -> Result<LuaEntrypointOutcome> {
    let request_table = build_v1_request_table(lua, request)
        .map_err(|e| anyhow!("Failed to build request: {e}"))?;
    let flow_store_ud = lua
        .create_userdata(LuaFlowStore::new(Arc::clone(&flow_store)))
        .map_err(|e| anyhow!("Failed to create flow_store userdata: {e}"))?;
    let ctx_table = build_ctx_table(lua, ctx_input, flow_store)
        .map_err(|e| anyhow!("Failed to build ctx: {e}"))?;
    register_result_constructors(lua).map_err(|e| anyhow!("Failed to register result API: {e}"))?;

    let globals = lua.globals();
    globals
        .set("request", request_table.clone())
        .map_err(|e| anyhow!("Failed to set request global: {e}"))?;
    globals
        .set("flow_store", flow_store_ud.clone())
        .map_err(|e| anyhow!("Failed to set flow_store global: {e}"))?;
    globals
        .set("ctx", ctx_table.clone())
        .map_err(|e| anyhow!("Failed to set ctx global: {e}"))?;

    // Snapshot the global names present BEFORE running the chunk, so afterwards we can tell which
    // function globals the SCRIPT itself declared (B1): Lua builtins plus the request/flow_store/
    // ctx/http/delay/reset/pass globals we set above are all already present here.
    let pre_existing: std::collections::HashSet<String> = globals
        .pairs::<LuaValue, LuaValue>()
        .filter_map(|pair| pair.ok())
        .filter_map(|(k, _)| lua_string_key(&k))
        .collect();

    let bare_result: LuaValue = chunk_fn
        .call(())
        .map_err(|e| anyhow!("Script execution error: {e}"))?;

    if entrypoint == entrypoints::RESPOND
        && let Ok(f) = globals.get::<LuaFunction>(entrypoints::SHOULD_INJECT)
    {
        let result: LuaValue = f
            .call((request_table, flow_store_ud))
            .map_err(|e| anyhow!("Failed to call should_inject: {e}"))?;
        return Ok(LuaEntrypointOutcome::V1(result));
    }

    if let Ok(f) = globals.get::<LuaFunction>(entrypoint) {
        let result: LuaValue = f
            .call(ctx_table)
            .map_err(|e| anyhow!("Failed to call {entrypoint}(ctx): {e}"))?;
        return Ok(LuaEntrypointOutcome::V2(result));
    }

    // B1 (issue #357, "nothing fails silently"): if the script DECLARED a function global that
    // isn't the requested entrypoint (nor `should_inject`) and produced no bare-expression value
    // (nil), it almost certainly has a MISNAMED entrypoint (e.g. `function respnod(ctx)`).
    // Falling through to `bare_result` (nil → `None`) would silently serve a normal response with
    // no sign the script never ran. Surface it as an explicit error instead. A genuine bare
    // expression is still fine: it either declared no new functions, or produced a non-nil value.
    if matches!(bare_result, LuaValue::Nil) {
        let declared_a_function = globals
            .pairs::<LuaValue, LuaValue>()
            .filter_map(|pair| pair.ok())
            .any(|(k, v)| {
                matches!(v, LuaValue::Function(_))
                    && lua_string_key(&k).is_some_and(|name| !pre_existing.contains(&name))
            });
        if declared_a_function {
            return Err(anyhow!(
                "script defines function(s) but none is the `{entrypoint}` entrypoint \
                 (and there is no bare expression to evaluate); did you mean `{entrypoint}`?"
            ));
        }
    }

    Ok(LuaEntrypointOutcome::V2(bare_result))
}

/// A Lua value's string key as an owned `String`, or `None` for non-string keys.
fn lua_string_key(k: &LuaValue) -> Option<String> {
    match k {
        LuaValue::String(s) => s.to_str().ok().map(|s| s.to_string()),
        _ => None,
    }
}

/// `respond(ctx)` (issue #357 Item 2): the response-script entrypoint. Auto-detects the v1
/// `should_inject` wrapper (issue #357 Item 4).
fn call_respond_lua(
    lua: &Lua,
    chunk_fn: &LuaFunction,
    request: &ScriptRequest,
    ctx_input: &ScriptCtxInput,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    match run_entrypoint_lua(
        lua,
        chunk_fn,
        entrypoints::RESPOND,
        request,
        ctx_input,
        flow_store,
    )? {
        LuaEntrypointOutcome::V1(v) => {
            let t: LuaTable = match v {
                LuaValue::Table(t) => t,
                _ => return Err(anyhow!("should_inject must return a table")),
            };
            parse_fault_decision_lua(lua, t, rule_id)
        }
        LuaEntrypointOutcome::V2(v) => lua_value_to_fault_decision(v, rule_id),
    }
}

/// Wrapper for FlowStore that can be used in Lua scripts.
///
/// The `strict` flag decides how a backend failure surfaces (issues #322/#376/#358):
/// - `strict == true` (the v2 `ctx.state` handle) — ALWAYS raise a Lua error on failure.
/// - `strict == false` (the legacy v1 `flow_store` global) — honor the `RIFT_STRICT_FLOW_STORE`
///   toggle (default lenient), preserving #322/#376 back-compat for v1 scripts.
struct LuaFlowStore {
    store: Arc<dyn FlowStore>,
    strict: bool,
}

impl LuaFlowStore {
    /// Legacy v1 `flow_store` global: fail-loud gated by the `RIFT_STRICT_FLOW_STORE` toggle.
    fn new(store: Arc<dyn FlowStore>) -> Self {
        Self {
            store,
            strict: strict_flow_store(),
        }
    }

    /// v2 `ctx.state` handle: ALWAYS fail-loud regardless of the env toggle (issue #358).
    fn new_strict(store: Arc<dyn FlowStore>) -> Self {
        Self {
            store,
            strict: true,
        }
    }

    /// Get a value from flow state. Fail-loud raises on a backend failure; otherwise returns nil
    /// (the lenient #322 fallback) and records the error for `last_error()`.
    fn get(&self, lua: &Lua, flow_id: String, key: String) -> LuaResult<LuaValue> {
        let store = Arc::clone(&self.store);

        // Direct synchronous call - no async bridging needed
        match flow_result("get", store.get(&flow_id, &key)) {
            Ok(Some(value)) => json_to_lua(lua, &value),
            Ok(None) => Ok(LuaValue::Nil),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(LuaValue::Nil),
        }
    }

    /// Set a value in flow state. Fail-loud raises on failure; else returns false (lenient #322).
    fn set(&self, lua: &Lua, flow_id: String, key: String, value: LuaValue) -> LuaResult<bool> {
        // Convert Lua value to JSON
        let json_value = lua_to_json(lua, value)?;

        let store = Arc::clone(&self.store);

        match flow_result("set", store.set(&flow_id, &key, json_value).map(|()| true)) {
            Ok(v) => Ok(v),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(false),
        }
    }

    /// Check if a key exists. Fail-loud raises on failure; else returns false (lenient #322).
    fn exists(&self, flow_id: String, key: String) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        match flow_result("exists", store.exists(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(false),
        }
    }

    /// Delete a key. Fail-loud raises on failure; else returns false (lenient #322).
    fn delete(&self, flow_id: String, key: String) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        match flow_result("delete", store.delete(&flow_id, &key).map(|()| true)) {
            Ok(v) => Ok(v),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(false),
        }
    }

    /// Increment a counter. Fail-loud raises on failure; else returns 0 (lenient #322).
    fn increment(&self, flow_id: String, key: String) -> LuaResult<i64> {
        let store = Arc::clone(&self.store);

        match flow_result("increment", store.increment(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(0),
        }
    }

    /// Set TTL for a flow. Fail-loud raises on failure; else returns false (lenient #322).
    fn set_ttl(&self, flow_id: String, ttl_seconds: i64) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        match flow_result(
            "setTtl",
            store.set_ttl(&flow_id, ttl_seconds).map(|()| true),
        ) {
            Ok(v) => Ok(v),
            Err(msg) if self.strict => Err(mlua::Error::runtime(msg)),
            Err(_) => Ok(false),
        }
    }

    /// Take (read and clear) the last flow-store op error for this thread, or nil if the
    /// last op succeeded (issue #322).
    fn last_error(&self) -> LuaResult<Option<String>> {
        Ok(crate::extensions::flow_state::take_last_flow_error())
    }

    // ============================================================
    // Atomic ops + ergonomic getters (issue #358). Unlike the ops above, these ALWAYS raise a Lua
    // error on a backend failure — fail-loud is the whole point of the new API, so a store outage
    // must never be conflated with "key absent"/"conflict" the way the lenient #322 fallback would.
    // ============================================================

    /// Get a value, or `default` if the key is absent. A store failure always raises.
    fn get_or(
        &self,
        lua: &Lua,
        flow_id: String,
        key: String,
        default: LuaValue,
    ) -> LuaResult<LuaValue> {
        let store = Arc::clone(&self.store);
        match flow_result("getOr", store.get(&flow_id, &key)) {
            Ok(Some(value)) => json_to_lua(lua, &value),
            Ok(None) => Ok(default),
            Err(msg) => Err(mlua::Error::runtime(msg)),
        }
    }

    /// Atomically increment by `by`, starting at 0 when absent. Always fail-loud.
    fn increment_by(&self, flow_id: String, key: String, by: i64) -> LuaResult<i64> {
        let store = Arc::clone(&self.store);
        flow_result("incrementBy", store.increment_by(&flow_id, &key, by))
            .map_err(mlua::Error::runtime)
    }

    /// Atomic compare-and-set (issue #358, #311). Returns a table `{ applied = true }` on success
    /// or `{ applied = false, current = <value-or-nil> }` on conflict — deliberately a table
    /// rather than a bare value so "conflict, current value happens to be `true`" can never be
    /// confused with "applied". Always fail-loud.
    fn cas(
        &self,
        lua: &Lua,
        flow_id: String,
        key: String,
        expected: LuaValue,
        new: LuaValue,
    ) -> LuaResult<LuaTable> {
        let expected_json = if matches!(expected, LuaValue::Nil) {
            None
        } else {
            Some(lua_to_json(lua, expected)?)
        };
        let new_json = lua_to_json(lua, new)?;
        let store = Arc::clone(&self.store);
        match flow_result(
            "cas",
            store.compare_and_set(&flow_id, &key, expected_json.as_ref(), new_json),
        ) {
            Ok(outcome) => cas_outcome_to_lua(lua, outcome),
            Err(msg) => Err(mlua::Error::runtime(msg)),
        }
    }

    /// Set a per-flow TTL override in seconds. Always fail-loud.
    fn ttl(&self, flow_id: String, ttl_seconds: i64) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);
        flow_result("ttl", store.set_ttl(&flow_id, ttl_seconds).map(|()| true))
            .map_err(mlua::Error::runtime)
    }
}

/// Convert a [`CasOutcome`] to the Lua return shape for `ctx.state:cas()` (issue #358): a table
/// with an `applied` flag and (on conflict) the winning `current` value, so success and conflict
/// are always structurally distinguishable — never just a bare value that could coincidentally
/// equal a "success" sentinel.
fn cas_outcome_to_lua(lua: &Lua, outcome: CasOutcome) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    match outcome {
        CasOutcome::Applied => {
            t.set("applied", true)?;
            t.set("current", LuaValue::Nil)?;
        }
        CasOutcome::Conflict(current) => {
            t.set("applied", false)?;
            let current_lua = match &current {
                Some(v) => json_to_lua(lua, v)?,
                None => LuaValue::Nil,
            };
            t.set("current", current_lua)?;
        }
    }
    Ok(t)
}

impl LuaUserData for LuaFlowStore {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, (flow_id, key): (String, String)| {
            this.get(lua, flow_id, key)
        });

        methods.add_method(
            "set",
            |lua, this, (flow_id, key, value): (String, String, LuaValue)| {
                this.set(lua, flow_id, key, value)
            },
        );

        methods.add_method("exists", |_lua, this, (flow_id, key): (String, String)| {
            this.exists(flow_id, key)
        });

        methods.add_method("delete", |_lua, this, (flow_id, key): (String, String)| {
            this.delete(flow_id, key)
        });

        methods.add_method(
            "increment",
            |_lua, this, (flow_id, key): (String, String)| this.increment(flow_id, key),
        );

        methods.add_method(
            "set_ttl",
            |_lua, this, (flow_id, ttl_seconds): (String, i64)| this.set_ttl(flow_id, ttl_seconds),
        );

        methods.add_method("last_error", |_lua, this, ()| this.last_error());
    }
}

/// Convert JSON Value to Lua value
fn json_to_lua(lua: &Lua, value: &Value) -> LuaResult<LuaValue> {
    match value {
        Value::Null => Ok(LuaValue::Nil),
        Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Ok(LuaValue::Nil)
            }
        }
        Value::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        Value::Object(obj) => {
            let table = lua.create_table()?;
            for (k, v) in obj {
                table.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

/// Convert Lua value to JSON Value
fn lua_to_json(_lua: &Lua, value: LuaValue) -> LuaResult<Value> {
    match value {
        LuaValue::Nil => Ok(Value::Null),
        LuaValue::Boolean(b) => Ok(Value::Bool(b)),
        LuaValue::Integer(i) => Ok(Value::Number(i.into())),
        LuaValue::Number(n) => {
            if let Some(num) = serde_json::Number::from_f64(n) {
                Ok(Value::Number(num))
            } else {
                Ok(Value::Null)
            }
        }
        LuaValue::String(s) => Ok(Value::String(s.to_str()?.to_string())),
        LuaValue::Table(table) => {
            // Check if it's an array (sequential integer keys starting from 1)
            let len = table.len()?;
            if len > 0 {
                let mut arr = Vec::new();
                for i in 1..=len {
                    let v: LuaValue = table.get(i)?;
                    arr.push(lua_to_json(_lua, v)?);
                }
                Ok(Value::Array(arr))
            } else {
                // It's an object
                let mut obj = serde_json::Map::new();
                for pair in table.pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair?;
                    if let LuaValue::String(key_str) = k {
                        obj.insert(key_str.to_str()?.to_string(), lua_to_json(_lua, v)?);
                    }
                }
                Ok(Value::Object(obj))
            }
        }
        _ => Ok(Value::Null),
    }
}

/// Run a Lua `should_inject` with a wall-clock interrupt hook (issue #308). An instruction
/// hook checks the `abort` flag periodically; when the caller's deadline sets it, the hook
/// returns an error, aborting the VM — the Lua analogue of Rhai's `on_progress` (#172).
pub fn run_should_inject_with_abort_lua(
    code: &str,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    abort: &Arc<AtomicBool>,
    ctx_extra: &ScriptCtxExtras,
) -> Result<FaultDecision> {
    let lua = Lua::new();
    // Disable JIT: LuaJIT trace-compiles hot loops (e.g. `while true do end`) into machine
    // code that bypasses the instruction hook, so a runaway script would never be
    // interrupted. Running the bytecode interpreter keeps the count hook firing (#308).
    if let Err(e) = lua.load("if jit then jit.off() end").exec() {
        // The whole interrupt mechanism relies on JIT being off; if this ever fails the
        // instruction hook may be bypassed, so make the degradation visible (issue #308).
        tracing::warn!("failed to disable LuaJIT for script interruption: {e}");
    }
    let flag = Arc::clone(abort);
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(2048),
        move |_lua, _debug| {
            if flag.load(Ordering::Relaxed) {
                Err(mlua::Error::runtime(
                    "script interrupted: execution timeout",
                ))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    );
    let bytecode = compile_to_bytecode(code)?;
    let chunk_fn = lua
        .load(&bytecode)
        .into_function()
        .map_err(|e| anyhow!("Failed to load bytecode: {e}"))?;
    let ctx_input = ctx_extra.build_ctx_input(request);
    call_respond_lua(&lua, &chunk_fn, request, &ctx_input, flow_store, rule_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::InMemoryFlowStore;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_lua_engine_compiles() {
        let script = r#"
function should_inject(request, flow_store)
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule");
        assert!(engine.is_ok());
    }

    #[tokio::test]
    async fn test_lua_engine_without_should_inject_still_constructs() {
        // Issue #357 Item 4: a script defining neither `should_inject` (v1) nor a v2 named
        // entrypoint is now legal — it's a v2 bare-expression script (Item 2). Construction only
        // validates that the script compiles.
        let script = r#"
function some_other_function()
    return true
end
"#;

        let engine = LuaEngine::new(script, "test-rule");
        assert!(
            engine.is_ok(),
            "bare/helper-only scripts must construct: {:?}",
            engine.err()
        );
    }

    #[tokio::test]
    async fn test_lua_engine_syntax_error_fails_construction() {
        let script = "function should_inject(request, flow_store {  -- missing paren";
        let engine = LuaEngine::new(script, "test-rule");
        assert!(
            engine.is_err(),
            "a genuine syntax error must still fail construction"
        );
    }

    #[tokio::test]
    async fn test_lua_simple_fault_injection() {
        let script = r#"
function should_inject(request, flow_store)
    if request.path == "/api/test" then
        return {
            inject = true,
            fault = "error",
            status = 503,
            body = "Service unavailable"
        }
    end
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    #[tokio::test]
    async fn test_lua_latency_fault() {
        let script = r#"
function should_inject(request, flow_store)
    return {
        inject = true,
        fault = "latency",
        duration_ms = 1000
    }
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    #[tokio::test]
    async fn test_lua_flow_store_increment() {
        let script = r#"
function should_inject(request, flow_store)
    local flow_id = request.headers["x-flow-id"] or ""
    if flow_id == "" then
        return {inject = false}
    end
    
    local attempts = flow_store:increment(flow_id, "attempts")
    
    if attempts <= 2 then
        return {
            inject = true,
            fault = "error",
            status = 503,
            body = "Attempt " .. attempts
        }
    end
    
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    /// A flow store whose `get` always fails, used to exercise `flow_store:last_error()`
    /// (issue #322). `should_inject` runs the script on its own spawned thread, so the
    /// failure must be recorded by the script itself (via a real failing op) rather than
    /// injected from the test thread's thread-local.
    #[derive(Debug)]
    struct FailingGetFlowStore;

    impl FlowStore for FailingGetFlowStore {
        fn get(&self, _flow_id: &str, _key: &str) -> anyhow::Result<Option<Value>> {
            Err(anyhow::anyhow!("lua backend down"))
        }
        fn set(&self, _flow_id: &str, _key: &str, _value: Value) -> anyhow::Result<()> {
            Ok(())
        }
        fn exists(&self, _flow_id: &str, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn delete(&self, _flow_id: &str, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn increment(&self, _flow_id: &str, _key: &str) -> anyhow::Result<i64> {
            Ok(0)
        }
        fn set_ttl(&self, _flow_id: &str, _ttl_seconds: i64) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // AC4 (issue #322): a Lua script can observe a backend flow-store failure via
    // flow_store:last_error() instead of only seeing a silent fallback value.
    #[tokio::test]
    async fn lua_flow_store_last_error_surfaces() {
        let script = r#"
function should_inject(request, flow_store)
    flow_store:get("flow-1", "key")
    local err = flow_store:last_error()
    if err and string.find(err, "lua backend down", 1, true) then
        return { inject = true, fault = "latency", duration_ms = 999 }
    end
    return { inject = true, fault = "latency", duration_ms = 1 }
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(FailingGetFlowStore);

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();
        match result {
            FaultDecision::Latency { duration_ms, .. } => assert_eq!(
                duration_ms, 999,
                "flow_store:last_error() must surface the recorded backend error to Lua"
            ),
            other => panic!("expected Latency decision surfacing last_error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_lua_flow_store_get_set() {
        let script = r#"
function should_inject(request, flow_store)
    local flow_id = request.headers["x-flow-id"] or ""
    if flow_id == "" then
        return {inject = false}
    end
    
    -- Set a value
    flow_store:set(flow_id, "test_key", "test_value")
    
    -- Get it back
    local value = flow_store:get(flow_id, "test_key")
    
    -- Check if it matches
    if value == "test_value" then
        return {
            inject = true,
            fault = "error",
            status = 200,
            body = "Get/Set works!"
        }
    end
    
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    #[tokio::test]
    async fn test_lua_state_reuse_optimization() {
        // Test that execute_lua_with_state can reuse Lua state across multiple invocations
        let script = r#"
function should_inject(request, flow_store)
    if request.path == "/api/test" then
        return {
            inject = true,
            fault = "latency",
            duration_ms = 100
        }
    end
    return {inject = false}
end
"#;

        let lua = Lua::new();
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

        // Execute the script multiple times with the same Lua state
        // This simulates what the script pool workers would do
        for i in 1..=5 {
            let result = execute_lua_with_state(
                &lua,
                script,
                &request,
                Arc::clone(&store),
                &format!("test-rule-{i}"),
            )
            .unwrap();

            match result {
                FaultDecision::Latency {
                    duration_ms,
                    rule_id,
                } => {
                    assert_eq!(duration_ms, 100);
                    assert_eq!(rule_id, format!("test-rule-{i}"));
                }
                _ => panic!("Expected latency fault decision on iteration {i}"),
            }
        }

        // Verify that we can execute a different request with the same state
        let request2 = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/other".to_string(),
            headers: HashMap::new(),
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result =
            execute_lua_with_state(&lua, script, &request2, store, "test-rule-final").unwrap();

        assert!(matches!(result, FaultDecision::None));
    }

    #[tokio::test]
    async fn test_lua_state_reuse_with_flow_store_isolation() {
        // Test that flow_store state is properly isolated between executions
        let script = r#"
function should_inject(request, flow_store)
    local flow_id = request.headers["x-flow-id"] or "default"
    local count = flow_store:increment(flow_id, "counter")
    
    if count > 3 then
        return {inject = false}
    end
    
    return {
        inject = true,
        fault = "error",
        status = 429,
        body = "Count: " .. count
    }
end
"#;

        let lua = Lua::new();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Execute with flow-A multiple times
        for i in 1..=3 {
            let mut headers = HashMap::new();
            headers.insert("x-flow-id".to_string(), "flow-A".to_string());

            let request = ScriptRequest {
                raw_body: None,
                method: "GET".to_string(),
                path: "/api/test".to_string(),
                headers,
                body: json!({}),
                query: HashMap::new(),
                path_params: HashMap::new(),
            };

            let result =
                execute_lua_with_state(&lua, script, &request, Arc::clone(&store), "test-rule")
                    .unwrap();

            match result {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 429);
                    assert_eq!(body, format!("Count: {i}"));
                }
                _ => panic!("Expected error fault decision for flow-A iteration {i}"),
            }
        }

        // Execute with flow-B - should start from count=1
        let mut headers_b = HashMap::new();
        headers_b.insert("x-flow-id".to_string(), "flow-B".to_string());

        let request_b = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: headers_b,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result_b =
            execute_lua_with_state(&lua, script, &request_b, Arc::clone(&store), "test-rule")
                .unwrap();

        match result_b {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 429);
                assert_eq!(body, "Count: 1");
            }
            _ => panic!("Expected error fault decision for flow-B"),
        }

        // Execute flow-A one more time - should exceed threshold
        let mut headers_a2 = HashMap::new();
        headers_a2.insert("x-flow-id".to_string(), "flow-A".to_string());

        let request_a2 = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: headers_a2,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let result_a2 =
            execute_lua_with_state(&lua, script, &request_a2, store, "test-rule").unwrap();

        assert!(matches!(result_a2, FaultDecision::None));
    }

    #[tokio::test]
    async fn test_lua_state_reuse_error_handling() {
        // Test that errors in one execution don't pollute the Lua state for subsequent executions
        let bad_script = r#"
function should_inject(request, flow_store)
    error("Intentional error")
end
"#;

        let good_script = r#"
function should_inject(request, flow_store)
    return {
        inject = true,
        fault = "latency",
        duration_ms = 50
    }
end
"#;

        let lua = Lua::new();
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

        // Execute bad script - should return error
        let result1 =
            execute_lua_with_state(&lua, bad_script, &request, Arc::clone(&store), "bad-rule");
        assert!(result1.is_err());
        assert!(result1.unwrap_err().to_string().contains("Intentional"));

        // Execute good script with same Lua state - should work fine
        let result2 =
            execute_lua_with_state(&lua, good_script, &request, Arc::clone(&store), "good-rule")
                .unwrap();

        match result2 {
            FaultDecision::Latency { duration_ms, .. } => {
                assert_eq!(duration_ms, 50);
            }
            _ => panic!("Expected latency fault decision after error recovery"),
        }

        // Execute good script again - should still work
        let result3 =
            execute_lua_with_state(&lua, good_script, &request, store, "good-rule-2").unwrap();

        assert!(matches!(result3, FaultDecision::Latency { .. }));
    }

    #[tokio::test]
    async fn test_lua_state_reuse_with_complex_body() {
        // Test that complex JSON bodies are handled correctly across reuses
        let script = r#"
function should_inject(request, flow_store)
    if request.body and request.body.nested and request.body.nested.value > 100 then
        return {
            inject = true,
            fault = "error",
            status = 400,
            body = "Value too high: " .. request.body.nested.value
        }
    end
    return {inject = false}
end
"#;

        let lua = Lua::new();
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

        let result1 =
            execute_lua_with_state(&lua, script, &request1, Arc::clone(&store), "rule-1").unwrap();

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

        let result2 = execute_lua_with_state(&lua, script, &request2, store, "rule-2").unwrap();

        assert!(matches!(result2, FaultDecision::None));
    }

    #[tokio::test]
    async fn test_compile_to_bytecode() {
        let script = r#"
function should_inject(request, flow_store)
    if request.path == "/test" then
        return {
            inject = true,
            fault = "error",
            status = 500,
            body = "Test error"
        }
    end
    return {inject = false}
end
"#;

        // Compile to bytecode
        let bytecode = compile_to_bytecode(script).unwrap();

        // Verify bytecode is not empty
        assert!(!bytecode.is_empty());

        // Bytecode should be smaller or similar size to source for simple scripts
        // (complex scripts might have larger bytecode)
        println!(
            "Source size: {} bytes, Bytecode size: {} bytes",
            script.len(),
            bytecode.len()
        );
    }

    #[tokio::test]
    async fn test_execute_lua_bytecode() {
        let script = r#"
function should_inject(request, flow_store)
    if request.path == "/api/bytecode" then
        return {
            inject = true,
            fault = "error",
            status = 503,
            body = "Bytecode executed"
        }
    end
    return {inject = false}
end
"#;

        // Compile to bytecode
        let bytecode = compile_to_bytecode(script).unwrap();

        // Create Lua state and execute bytecode
        let lua = Lua::new();
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

        let result =
            execute_lua_bytecode(&lua, &bytecode, &request, store, "bytecode-rule").unwrap();

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
                assert!(headers.is_empty()); // No headers in this test
            }
            _ => panic!("Expected error fault decision from bytecode execution"),
        }
    }

    #[tokio::test]
    async fn test_bytecode_reuse() {
        let script = r#"
function should_inject(request, flow_store)
    local flow_id = request.headers["x-flow-id"] or "default"
    local count = flow_store:increment(flow_id, "count")
    
    if count > 2 then
        return {
            inject = true,
            fault = "error",
            status = 429,
            body = "Rate limited"
        }
    end
    return {inject = false}
end
"#;

        // Compile once
        let bytecode = compile_to_bytecode(script).unwrap();

        // Execute multiple times with the same bytecode (simulating pool worker)
        let lua = Lua::new();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow-456".to_string());

        let request = ScriptRequest {
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        // First two executions should pass
        let result1 =
            execute_lua_bytecode(&lua, &bytecode, &request, Arc::clone(&store), "rule-1").unwrap();
        assert!(matches!(result1, FaultDecision::None));

        let result2 =
            execute_lua_bytecode(&lua, &bytecode, &request, Arc::clone(&store), "rule-1").unwrap();
        assert!(matches!(result2, FaultDecision::None));

        // Third execution should trigger rate limit
        let result3 = execute_lua_bytecode(&lua, &bytecode, &request, store, "rule-1").unwrap();
        assert!(matches!(result3, FaultDecision::Error { status: 429, .. }));
    }

    #[tokio::test]
    async fn test_lua_query_params() {
        let script = r#"
function should_inject(request, flow_store)
    local name = request.query["name"]
    local page = request.query["page"]

    if name and page then
        return {
            inject = true,
            fault = "error",
            status = 200,
            body = "Hello " .. name .. " on page " .. page
        }
    end
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    #[tokio::test]
    async fn test_lua_path_params() {
        let script = r#"
function should_inject(request, flow_store)
    local user_id = request.pathParams["id"]
    local action = request.pathParams["action"]

    if user_id and action then
        return {
            inject = true,
            fault = "error",
            status = 200,
            body = "User " .. user_id .. " action: " .. action
        }
    end
    return {inject = false}
end
"#;

        let engine = LuaEngine::new(script, "test-rule").unwrap();
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

    // ============================================
    // Issue #357: unified ctx, v2 respond entrypoint, result constructors (Lua)
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
            let engine = LuaEngine::new(script, "v2-rule").unwrap();
            engine.should_inject(request, store())
        }

        #[tokio::test]
        async fn respond_named_wrapper() {
            let script = r#"
                function respond(ctx)
                    return http(503, { error = "unavailable" })
                end
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

        #[tokio::test]
        async fn respond_bare_expression() {
            let script = r#"
                if ctx.request.method == "POST" then
                    return http(503, { error = "unavailable" })
                else
                    return pass()
                end
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        #[tokio::test]
        async fn ctx_request_header_case_insensitive() {
            let headers = HashMap::from([("X-Flow-Id".to_string(), "flow-9".to_string())]);
            let script = r#"
                function respond(ctx)
                    local v = ctx.request:header("x-flow-id")
                    return http(200, { seen = v })
                end
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

        #[tokio::test]
        async fn ctx_request_json_lazy_parse() {
            let script = r#"
                function respond(ctx)
                    if ctx.request.json == nil then
                        return http(500)
                    end
                    return http(200, { n = ctx.request.json.n })
                end
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

        #[tokio::test]
        async fn result_constructors_delay_reset_pass() {
            let delay = run_respond(
                "function respond(ctx) return delay(42) end",
                &req(HashMap::new(), None),
            )
            .unwrap();
            match delay {
                FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 42),
                other => panic!("expected Latency, got {other:?}"),
            }

            let reset = run_respond(
                "function respond(ctx) return reset() end",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(reset, FaultDecision::Reset { .. }));

            let pass = run_respond(
                "function respond(ctx) return pass() end",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(pass, FaultDecision::None));

            let nothing =
                run_respond("function respond(ctx) end", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(nothing, FaultDecision::None));
        }

        #[tokio::test]
        async fn http_string_body_passes_through_verbatim() {
            let script = r#"function respond(ctx) return http(200, "hello world") end"#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, headers, .. } => {
                    assert_eq!(body, "hello world");
                    assert!(!headers.contains_key("Content-Type"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        // v1 back-compat: `should_inject` still wins (issue #357 Item 4).
        #[tokio::test]
        async fn v1_should_inject_still_works_unchanged() {
            let script = r#"
                function should_inject(request, flow_store)
                    return { inject = true, fault = "error", status = 418, body = "teapot" }
                end
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

        // Retry example from the issue: ctx.state:incr + http() executes end-to-end.
        #[tokio::test]
        async fn retry_example_end_to_end_with_ctx_state_incr() {
            let script = r#"
                function respond(ctx)
                    local n = ctx.state:incr("attempts")
                    if n <= 2 then
                        return http(503, { error = "unavailable", attempt = n }):header("Retry-After", "1")
                    end
                    return http(200, { ok = true, succeededOnAttempt = n })
                end
            "#;
            let engine = LuaEngine::new(script, "retry-rule").unwrap();
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

        #[tokio::test]
        async fn get_or_returns_default_when_absent_then_stored_value() {
            let script = r#"
                function respond(ctx)
                    local first = ctx.state:get_or("count", 0)
                    ctx.state:set("count", 7)
                    local second = ctx.state:get_or("count", 0)
                    return http(200, { first = first, second = second })
                end
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

        #[tokio::test]
        async fn incr_by_is_atomic_and_starts_at_zero() {
            let script = r#"
                function respond(ctx)
                    local n = ctx.state:incr_by("hits", 5)
                    return http(200, { n = n })
                end
            "#;
            let engine = LuaEngine::new(script, "incr-by-rule").unwrap();
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

        #[tokio::test]
        async fn cas_distinguishes_applied_from_conflict() {
            let shared_store = store();
            let request = req(HashMap::new(), None);

            // First call: key "status" absent, expected nil matches -> applied, sets "paid".
            let applied_script = r#"
                function respond(ctx)
                    local outcome = ctx.state:cas("status", nil, "paid")
                    return http(200, { applied = outcome.applied, current = outcome.current })
                end
            "#;
            let applied_engine = LuaEngine::new(applied_script, "cas-applied").unwrap();
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
                function respond(ctx)
                    local outcome = ctx.state:cas("status", "pending", "shipped")
                    return http(200, { applied = outcome.applied, current = outcome.current })
                end
            "#;
            let conflict_engine = LuaEngine::new(conflict_script, "cas-conflict").unwrap();
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

        #[tokio::test]
        async fn ttl_sets_per_flow_expiry() {
            let script = r#"
                function respond(ctx)
                    ctx.state:set("k", 1)
                    local applied = ctx.state:ttl(3600)
                    return http(200, { applied = applied })
                end
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":true"), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // Issue #358 B3 (AC4): a backend failure on any atomic op must propagate as a script error.
        #[cfg(feature = "test-backend")]
        #[tokio::test]
        async fn atomic_ops_propagate_store_errors_fail_loud() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            for (name, script) in [
                (
                    "get_or",
                    r#"function respond(ctx) return http(200, { v = ctx.state:get_or("k", 0) }) end"#,
                ),
                (
                    "incr_by",
                    r#"function respond(ctx) return http(200, { v = ctx.state:incr_by("k", 1) }) end"#,
                ),
                (
                    "cas",
                    r#"function respond(ctx) return http(200, { v = ctx.state:cas("k", nil, "v").applied }) end"#,
                ),
                (
                    "ttl",
                    r#"function respond(ctx) return http(200, { v = ctx.state:ttl(60) }) end"#,
                ),
            ] {
                let result = LuaEngine::new(script, "fail")
                    .unwrap()
                    .should_inject(&request, Arc::new(FailingFlowStore));
                assert!(result.is_err(), "{name} must propagate a store failure");
            }
        }

        // Issue #358 B1 / #322: pre-existing v2 ops (get/incr) fail loud even with the toggle
        // unset, while the legacy v1 `flow_store` global stays lenient under the default toggle.
        #[cfg(feature = "test-backend")]
        #[tokio::test]
        async fn v2_state_ops_fail_loud_while_v1_flow_store_lenient() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            let get_err = LuaEngine::new(
                r#"function respond(ctx) return http(200, { v = ctx.state:get("k") }) end"#,
                "v2-get",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(get_err.is_err(), "v2 ctx.state:get must fail loud");

            let incr_err = LuaEngine::new(
                r#"function respond(ctx) return http(200, { v = ctx.state:incr("k") }) end"#,
                "v2-incr",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(incr_err.is_err(), "v2 ctx.state:incr must fail loud");

            // v1 flow_store:get stays lenient under the default (unset) toggle — no raise.
            let v1 = LuaEngine::new(
                r#"function should_inject(request, flow_store) flow_store:get("f", "k"); return { inject = false } end"#,
                "v1-get",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(
                matches!(v1, Ok(FaultDecision::None)),
                "v1 flow_store:get must stay lenient under the default toggle, got {v1:?}"
            );
        }

        // B1 (issue #357): a script defining ONLY a misnamed entrypoint must Err, not None.
        #[tokio::test]
        async fn misnamed_entrypoint_errors_not_none() {
            let script = r#"
                function respnod(ctx)
                    return http(500)
                end
            "#;
            let result = LuaEngine::new(script, "v2-rule")
                .unwrap()
                .should_inject(&req(HashMap::new(), None), store());
            assert!(
                result.is_err(),
                "a misnamed entrypoint must surface an error, got {result:?}"
            );
        }

        #[tokio::test]
        async fn genuine_bare_expression_still_ok_after_b1() {
            let decision = run_respond("return http(503)", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        // ctx.request.pathParams and ctx.request.query round-trip (mirrors the Rhai test).
        #[tokio::test]
        async fn ctx_request_path_params_and_query() {
            let mut request = req(HashMap::new(), None);
            request
                .path_params
                .insert("id".to_string(), "42".to_string());
            request.query.insert("page".to_string(), "2".to_string());
            let script = r#"
                function respond(ctx)
                    return http(200, { id = ctx.request.pathParams.id, page = ctx.request.query.page })
                end
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
    }
}
