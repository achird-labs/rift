use crate::extensions::flow_state::FlowStore;
use crate::scripting::{FaultDecision, ScriptRequest};
use anyhow::{anyhow, Result};
use mlua::prelude::*;
use serde_json::Value;
use std::sync::Arc;

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
    pub fn new(script: &str, rule_id: String) -> Result<Self> {
        // Validate script compiles
        let lua = Lua::new();
        lua.load(script)
            .exec()
            .map_err(|e| anyhow!("Failed to compile Lua script: {e}"))?;

        // Check that should_inject function exists
        let globals = lua.globals();
        let func: LuaResult<LuaFunction> = globals.get("should_inject");
        if func.is_err() {
            return Err(anyhow!("Script must define should_inject function"));
        }

        Ok(Self {
            script: script.to_string(),
            rule_id,
        })
    }

    pub fn should_inject(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        // DEPRECATED: This method spawns a thread+runtime per execution (expensive!)
        // Script pool workers should use execute_lua_with_state() instead
        // Kept for backward compatibility only
        let script = self.script.clone();
        let request = request.clone();
        let rule_id = self.rule_id.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            // Create a new runtime for this thread to handle async FlowStore calls
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            let _guard = rt.enter();

            let result = Self::execute_in_thread(script, request, flow_store, rule_id);
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|_| anyhow!("Lua execution thread panicked"))?
    }

    fn execute_in_thread(
        script: String,
        request: ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        rule_id: String,
    ) -> Result<FaultDecision> {
        let lua = Lua::new();

        // Create request table
        let request_table = lua
            .create_table()
            .map_err(|e| anyhow!("Failed to create request table: {e}"))?;
        request_table
            .set("method", request.method.clone())
            .map_err(|e| anyhow!("Failed to set method: {e}"))?;
        request_table
            .set("path", request.path.clone())
            .map_err(|e| anyhow!("Failed to set path: {e}"))?;

        // Create headers table
        let headers_table = lua
            .create_table()
            .map_err(|e| anyhow!("Failed to create headers table: {e}"))?;
        for (k, v) in &request.headers {
            headers_table
                .set(k.as_str(), v.as_str())
                .map_err(|e| anyhow!("Failed to set header: {e}"))?;
        }
        request_table
            .set("headers", headers_table)
            .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

        // Convert body JSON to Lua value
        let body_value =
            json_to_lua(&lua, &request.body).map_err(|e| anyhow!("Failed to convert body: {e}"))?;
        request_table
            .set("body", body_value)
            .map_err(|e| anyhow!("Failed to set body: {e}"))?;

        // Create query parameters table
        let query_table = lua
            .create_table()
            .map_err(|e| anyhow!("Failed to create query table: {e}"))?;
        for (k, v) in &request.query {
            query_table
                .set(k.as_str(), v.as_str())
                .map_err(|e| anyhow!("Failed to set query param: {e}"))?;
        }
        request_table
            .set("query", query_table)
            .map_err(|e| anyhow!("Failed to set query: {e}"))?;

        // Create path parameters table
        let path_params_table = lua
            .create_table()
            .map_err(|e| anyhow!("Failed to create path_params table: {e}"))?;
        for (k, v) in &request.path_params {
            path_params_table
                .set(k.as_str(), v.as_str())
                .map_err(|e| anyhow!("Failed to set path param: {e}"))?;
        }
        request_table
            .set("pathParams", path_params_table)
            .map_err(|e| anyhow!("Failed to set pathParams: {e}"))?;

        // Create flow_store userdata
        let flow_store_ud = lua
            .create_userdata(LuaFlowStore::new(flow_store))
            .map_err(|e| anyhow!("Failed to create flow_store userdata: {e}"))?;

        // Set global variables
        let globals = lua.globals();
        globals
            .set("request", request_table)
            .map_err(|e| anyhow!("Failed to set request global: {e}"))?;
        globals
            .set("flow_store", flow_store_ud)
            .map_err(|e| anyhow!("Failed to set flow_store global: {e}"))?;

        // Load and execute script
        lua.load(&script)
            .exec()
            .map_err(|e| anyhow!("Failed to execute script: {e}"))?;

        // Call should_inject function
        let should_inject: LuaFunction = globals
            .get("should_inject")
            .map_err(|e| anyhow!("Failed to get should_inject function: {e}"))?;
        let request_arg: LuaTable = globals
            .get("request")
            .map_err(|e| anyhow!("Failed to get request: {e}"))?;
        let flow_store_arg: LuaAnyUserData = globals
            .get("flow_store")
            .map_err(|e| anyhow!("Failed to get flow_store: {e}"))?;
        let result: LuaTable = should_inject
            .call((request_arg, flow_store_arg))
            .map_err(|e| anyhow!("Failed to call should_inject: {e}"))?;

        // Parse result table
        Self::parse_fault_decision(&lua, result, rule_id)
    }

    fn parse_fault_decision(
        _lua: &Lua,
        result: LuaTable,
        rule_id: String,
    ) -> Result<FaultDecision> {
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
                    rule_id,
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
                    rule_id,
                    headers,
                })
            }
            _ => Ok(FaultDecision::None),
        }
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

/// Public function to execute Lua bytecode with a reusable Lua state (for script pool)
/// This eliminates the expensive compilation overhead on each execution
pub fn execute_lua_bytecode(
    lua: &Lua,
    bytecode: &[u8],
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    // Create request table
    let request_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create request table: {e}"))?;
    request_table
        .set("method", request.method.clone())
        .map_err(|e| anyhow!("Failed to set method: {e}"))?;
    request_table
        .set("path", request.path.clone())
        .map_err(|e| anyhow!("Failed to set path: {e}"))?;

    // Create headers table
    let headers_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create headers table: {e}"))?;
    for (k, v) in &request.headers {
        headers_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    request_table
        .set("headers", headers_table)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // Convert body JSON to Lua value
    let body_value =
        json_to_lua(lua, &request.body).map_err(|e| anyhow!("Failed to convert body: {e}"))?;
    request_table
        .set("body", body_value)
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;

    // Create query parameters table
    let query_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create query table: {e}"))?;
    for (k, v) in &request.query {
        query_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set query param: {e}"))?;
    }
    request_table
        .set("query", query_table)
        .map_err(|e| anyhow!("Failed to set query: {e}"))?;

    // Create path parameters table
    let path_params_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create path_params table: {e}"))?;
    for (k, v) in &request.path_params {
        path_params_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set path param: {e}"))?;
    }
    request_table
        .set("pathParams", path_params_table)
        .map_err(|e| anyhow!("Failed to set pathParams: {e}"))?;

    // Create flow_store userdata
    let flow_store_ud = lua
        .create_userdata(LuaFlowStore::new(flow_store))
        .map_err(|e| anyhow!("Failed to create flow_store userdata: {e}"))?;

    // Set global variables
    let globals = lua.globals();
    globals
        .set("request", request_table)
        .map_err(|e| anyhow!("Failed to set request global: {e}"))?;
    globals
        .set("flow_store", flow_store_ud)
        .map_err(|e| anyhow!("Failed to set flow_store global: {e}"))?;

    // Load and execute bytecode (MUCH faster than compiling from source)
    lua.load(bytecode)
        .exec()
        .map_err(|e| anyhow!("Failed to execute bytecode: {e}"))?;

    // Call should_inject function
    let should_inject: LuaFunction = globals
        .get("should_inject")
        .map_err(|e| anyhow!("Failed to get should_inject function: {e}"))?;
    let request_arg: LuaTable = globals
        .get("request")
        .map_err(|e| anyhow!("Failed to get request: {e}"))?;
    let flow_store_arg: LuaAnyUserData = globals
        .get("flow_store")
        .map_err(|e| anyhow!("Failed to get flow_store: {e}"))?;
    let result: LuaTable = should_inject
        .call((request_arg, flow_store_arg))
        .map_err(|e| anyhow!("Failed to call should_inject: {e}"))?;

    // Parse result table with rule_id parameter
    parse_fault_decision_lua(lua, result, rule_id)
}

/// Public function to execute Lua script with a reusable Lua state (for script pool)
/// This eliminates the expensive thread spawning and runtime creation overhead
pub fn execute_lua_with_state(
    lua: &Lua,
    script: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    // Create request table
    let request_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create request table: {e}"))?;
    request_table
        .set("method", request.method.clone())
        .map_err(|e| anyhow!("Failed to set method: {e}"))?;
    request_table
        .set("path", request.path.clone())
        .map_err(|e| anyhow!("Failed to set path: {e}"))?;

    // Create headers table
    let headers_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create headers table: {e}"))?;
    for (k, v) in &request.headers {
        headers_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    request_table
        .set("headers", headers_table)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // Convert body JSON to Lua value
    let body_value =
        json_to_lua(lua, &request.body).map_err(|e| anyhow!("Failed to convert body: {e}"))?;
    request_table
        .set("body", body_value)
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;

    // Create query parameters table
    let query_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create query table: {e}"))?;
    for (k, v) in &request.query {
        query_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set query param: {e}"))?;
    }
    request_table
        .set("query", query_table)
        .map_err(|e| anyhow!("Failed to set query: {e}"))?;

    // Create path parameters table
    let path_params_table = lua
        .create_table()
        .map_err(|e| anyhow!("Failed to create path_params table: {e}"))?;
    for (k, v) in &request.path_params {
        path_params_table
            .set(k.as_str(), v.as_str())
            .map_err(|e| anyhow!("Failed to set path param: {e}"))?;
    }
    request_table
        .set("pathParams", path_params_table)
        .map_err(|e| anyhow!("Failed to set pathParams: {e}"))?;

    // Create flow_store userdata
    let flow_store_ud = lua
        .create_userdata(LuaFlowStore::new(flow_store))
        .map_err(|e| anyhow!("Failed to create flow_store userdata: {e}"))?;

    // Set global variables
    let globals = lua.globals();
    globals
        .set("request", request_table)
        .map_err(|e| anyhow!("Failed to set request global: {e}"))?;
    globals
        .set("flow_store", flow_store_ud)
        .map_err(|e| anyhow!("Failed to set flow_store global: {e}"))?;

    // Load and execute script (from string for now, bytecode could be added later)
    lua.load(script)
        .exec()
        .map_err(|e| anyhow!("Failed to execute script: {e}"))?;

    // Call should_inject function
    let should_inject: LuaFunction = globals
        .get("should_inject")
        .map_err(|e| anyhow!("Failed to get should_inject function: {e}"))?;
    let request_arg: LuaTable = globals
        .get("request")
        .map_err(|e| anyhow!("Failed to get request: {e}"))?;
    let flow_store_arg: LuaAnyUserData = globals
        .get("flow_store")
        .map_err(|e| anyhow!("Failed to get flow_store: {e}"))?;
    let result: LuaTable = should_inject
        .call((request_arg, flow_store_arg))
        .map_err(|e| anyhow!("Failed to call should_inject: {e}"))?;

    // Parse result table with rule_id parameter
    parse_fault_decision_lua(lua, result, rule_id)
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

/// Wrapper for FlowStore that can be used in Lua scripts
struct LuaFlowStore {
    store: Arc<dyn FlowStore>,
}

impl LuaFlowStore {
    fn new(store: Arc<dyn FlowStore>) -> Self {
        Self { store }
    }

    /// Get a value from flow state
    fn get(&self, lua: &Lua, flow_id: String, key: String) -> LuaResult<LuaValue> {
        let store = Arc::clone(&self.store);

        // Direct synchronous call - no async bridging needed
        match store.get(&flow_id, &key) {
            Ok(Some(value)) => json_to_lua(lua, &value),
            _ => Ok(LuaValue::Nil),
        }
    }

    /// Set a value in flow state
    fn set(&self, lua: &Lua, flow_id: String, key: String, value: LuaValue) -> LuaResult<bool> {
        // Convert Lua value to JSON
        let json_value = lua_to_json(lua, value)?;

        let store = Arc::clone(&self.store);

        let result = store.set(&flow_id, &key, json_value);

        Ok(result.is_ok())
    }

    /// Check if a key exists
    fn exists(&self, flow_id: String, key: String) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        match store.exists(&flow_id, &key) {
            Ok(exists) => Ok(exists),
            Err(_) => Ok(false),
        }
    }

    /// Delete a key
    fn delete(&self, flow_id: String, key: String) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        let result = store.delete(&flow_id, &key);

        Ok(result.is_ok())
    }

    /// Increment a counter
    fn increment(&self, flow_id: String, key: String) -> LuaResult<i64> {
        let store = Arc::clone(&self.store);

        match store.increment(&flow_id, &key) {
            Ok(value) => Ok(value),
            Err(_) => Ok(0),
        }
    }

    /// Set TTL for a flow
    fn set_ttl(&self, flow_id: String, ttl_seconds: i64) -> LuaResult<bool> {
        let store = Arc::clone(&self.store);

        let result = store.set_ttl(&flow_id, ttl_seconds);

        Ok(result.is_ok())
    }
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

        let engine = LuaEngine::new(script, "test-rule".to_string());
        assert!(engine.is_ok());
    }

    #[tokio::test]
    async fn test_lua_engine_missing_function() {
        let script = r#"
function some_other_function()
    return true
end
"#;

        let engine = LuaEngine::new(script, "test-rule".to_string());
        assert!(engine.is_err());
        assert!(engine.unwrap_err().to_string().contains("should_inject"));
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request = ScriptRequest {
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow-123".to_string());

        let request = ScriptRequest {
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow-123".to_string());

        let request = ScriptRequest {
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut query = HashMap::new();
        query.insert("name".to_string(), "Alice".to_string());
        query.insert("page".to_string(), "42".to_string());

        let request = ScriptRequest {
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

        let engine = LuaEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "123".to_string());
        path_params.insert("action".to_string(), "update".to_string());

        let request = ScriptRequest {
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
}
