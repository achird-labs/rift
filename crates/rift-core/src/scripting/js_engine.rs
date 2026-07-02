use crate::extensions::flow_state::FlowStore;
use crate::scripting::{FaultDecision, ScriptRequest};
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
use std::cell::RefCell;
use std::sync::Arc;

// Thread-local storage for the flow store during script execution
thread_local! {
    static CURRENT_FLOW_STORE: RefCell<Option<Arc<dyn FlowStore>>> = const { RefCell::new(None) };
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
/// # Script Interface
///
/// Scripts must define a `should_inject` function with the following signature:
///
/// ```javascript
/// function should_inject(request, flow_store) {
///     // Your logic here
///     return { inject: false };
/// }
/// ```
///
/// ## Request Object
///
/// The `request` parameter is an object containing:
/// - `method` - HTTP method (string): "GET", "POST", "PUT", "DELETE", etc.
/// - `path` - Request path (string): "/api/users/123"
/// - `headers` - Object of header name (string) to value (string)
/// - `body` - Request body (parsed JSON value or null)
/// - `query` - Object of query parameter name (string) to value (string)
/// - `pathParams` - Object of path parameters extracted from route patterns
///
/// ## Flow Store Object
///
/// The `flow_store` parameter provides state management across requests:
/// - `flow_store.get(flow_id, key)` - Get a stored value (returns null if not found)
/// - `flow_store.set(flow_id, key, value)` - Store a value (returns boolean)
/// - `flow_store.exists(flow_id, key)` - Check if key exists (returns boolean)
/// - `flow_store.delete(flow_id, key)` - Delete a key (returns boolean)
/// - `flow_store.increment(flow_id, key)` - Increment counter (returns number)
/// - `flow_store.set_ttl(flow_id, ttl_seconds)` - Set flow expiration (returns boolean)
///
/// ## Return Value
///
/// The function must return an object with the fault decision:
///
/// ```javascript
/// // No fault injection
/// { inject: false }
///
/// // Latency injection
/// { inject: true, fault: "latency", duration_ms: 500 }
///
/// // Error injection
/// { inject: true, fault: "error", status: 503, body: "Service unavailable" }
///
/// // Error with custom headers
/// {
///     inject: true,
///     fault: "error",
///     status: 429,
///     body: "Rate limited",
///     headers: { "Retry-After": "60" }
/// }
/// ```
///
/// ## Example
///
/// ```javascript
/// function should_inject(request, flow_store) {
///     // Rate limit based on flow ID from header
///     var flow_id = request.headers["x-flow-id"];
///     if (flow_id) {
///         var attempts = flow_store.increment(flow_id, "attempts");
///         if (attempts > 3) {
///             return { inject: true, fault: "error", status: 429, body: "Rate limited" };
///         }
///     }
///
///     // Inject fault for POST requests to specific path
///     if (request.method === "POST" && request.path === "/api/test") {
///         return { inject: true, fault: "latency", duration_ms: 100 };
///     }
///
///     return { inject: false };
/// }
/// ```
#[derive(Debug, Clone)]
pub struct JsEngine {
    script: String,
    rule_id: String,
}

impl JsEngine {
    pub fn new(script: &str, rule_id: &str) -> Result<Self> {
        // Validate script compiles by evaluating it
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(script.as_bytes()))
            .map_err(|e| anyhow!("Failed to compile JavaScript script: {e}"))?;

        // Check that should_inject function exists
        let global = context.global_object();
        let func = global.get(js_string!("should_inject"), &mut context);
        match func {
            Ok(val) if val.is_callable() => {}
            _ => {
                return Err(anyhow!("Script must define should_inject function"));
            }
        }

        Ok(Self {
            script: script.to_string(),
            rule_id: rule_id.to_string(),
        })
    }

    pub fn should_inject(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        execute_js_script(&self.script, request, flow_store, &self.rule_id)
    }
}

/// Compile JavaScript to bytecode for caching
/// Returns serialized bytecode that can be loaded efficiently
/// Note: Boa doesn't support bytecode serialization yet, so we store the source
/// and validate it compiles
// Used by proxy.rs but cross-module analysis doesn't see it
pub fn compile_js_to_bytecode(script: &str) -> Result<Vec<u8>> {
    // Validate the script compiles
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(script.as_bytes()))
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
    execute_js_script(script, request, flow_store, rule_id)
}

/// Internal function to execute JavaScript script
fn execute_js_script(
    script: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    // Set the flow store in thread-local storage for native functions to access
    set_current_flow_store(flow_store);

    // Ensure we clear the flow store when done (even on error)
    let result = execute_js_script_inner(script, request, rule_id);

    clear_current_flow_store();

    result
}

/// Inner function that does the actual JavaScript execution
fn execute_js_script_inner(
    script: &str,
    request: &ScriptRequest,
    rule_id: &str,
) -> Result<FaultDecision> {
    let mut context = Context::default();

    // Create request object
    let request_obj = create_request_object(&mut context, request)?;

    // Create flow_store object with methods
    let flow_store_obj = create_flow_store_object(&mut context)?;

    // Set global variables
    let global = context.global_object();
    global
        .set(js_string!("request"), request_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set request global: {e}"))?;
    global
        .set(
            js_string!("flow_store"),
            flow_store_obj,
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set flow_store global: {e}"))?;

    // Execute script to define the function
    context
        .eval(Source::from_bytes(script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute script: {e}"))?;

    // Call should_inject function
    let func = global
        .get(js_string!("should_inject"), &mut context)
        .map_err(|e| anyhow!("Failed to get should_inject function: {e}"))?;

    let request_arg = global
        .get(js_string!("request"), &mut context)
        .map_err(|e| anyhow!("Failed to get request: {e}"))?;
    let flow_store_arg = global
        .get(js_string!("flow_store"), &mut context)
        .map_err(|e| anyhow!("Failed to get flow_store: {e}"))?;

    let result = func
        .as_callable()
        .ok_or_else(|| anyhow!("should_inject is not a function"))?
        .call(
            &JsValue::undefined(),
            &[request_arg, flow_store_arg],
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to call should_inject: {e}"))?;

    // Parse result
    parse_fault_decision(&mut context, result, rule_id)
}

/// Create request object from ScriptRequest
fn create_request_object(context: &mut Context, request: &ScriptRequest) -> Result<JsValue> {
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

    // Set body
    let body_value = json_to_js(context, &request.body)?;
    obj.set(js_string!("body"), body_value, false, context)
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;

    // Set query parameters
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

    // Set path parameters
    let path_params_obj = create_js_object(context);
    for (k, v) in &request.path_params {
        path_params_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set path param: {e}"))?;
    }
    obj.set(js_string!("pathParams"), path_params_obj, false, context)
        .map_err(|e| anyhow!("Failed to set pathParams: {e}"))?;

    Ok(obj.into())
}

// Native function implementations that use thread-local storage

fn flow_store_get(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let key = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;

    let result = with_current_flow_store(|store| store.get(&flow_id, &key));

    match result {
        Some(Ok(Some(value))) => json_to_js_result(ctx, &value),
        _ => Ok(JsValue::null()),
    }
}

fn flow_store_set(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let key = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let value = args.get(2).cloned().unwrap_or(JsValue::null());

    let json_value = js_to_json(ctx, &value)?;
    let result = with_current_flow_store(|store| store.set(&flow_id, &key, json_value).is_ok())
        .unwrap_or(false);

    Ok(JsValue::from(result))
}

fn flow_store_exists(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let key = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;

    let result = with_current_flow_store(|store| store.exists(&flow_id, &key).unwrap_or(false))
        .unwrap_or(false);

    Ok(JsValue::from(result))
}

fn flow_store_delete(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let key = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;

    let result =
        with_current_flow_store(|store| store.delete(&flow_id, &key).is_ok()).unwrap_or(false);

    Ok(JsValue::from(result))
}

fn flow_store_increment(
    _this: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let key = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;

    let result =
        with_current_flow_store(|store| store.increment(&flow_id, &key).unwrap_or(0)).unwrap_or(0);

    Ok(JsValue::from(result))
}

fn flow_store_set_ttl(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    let ttl_seconds = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n as i64)
        .ok_or_else(|| JsNativeError::typ().with_message("ttl_seconds must be a number"))?;

    let result = with_current_flow_store(|store| store.set_ttl(&flow_id, ttl_seconds).is_ok())
        .unwrap_or(false);

    Ok(JsValue::from(result))
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

/// Create flow_store object with methods using thread-local storage
fn create_flow_store_object(context: &mut Context) -> Result<JsValue> {
    let obj = create_js_object(context);

    register_method(&obj, "get", flow_store_get, context)?;
    register_method(&obj, "set", flow_store_set, context)?;
    register_method(&obj, "exists", flow_store_exists, context)?;
    register_method(&obj, "delete", flow_store_delete, context)?;
    register_method(&obj, "increment", flow_store_increment, context)?;
    register_method(&obj, "set_ttl", flow_store_set_ttl, context)?;

    Ok(obj.into())
}

/// Parse fault decision from JavaScript result
fn parse_fault_decision(
    context: &mut Context,
    result: JsValue,
    rule_id: &str,
) -> Result<FaultDecision> {
    if result.is_null() || result.is_undefined() {
        return Ok(FaultDecision::None);
    }

    let obj = result
        .as_object()
        .ok_or_else(|| anyhow!("Script must return an object"))?;

    // Check inject flag
    let inject = obj
        .get(js_string!("inject"), context)
        .ok()
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);

    if !inject {
        return Ok(FaultDecision::None);
    }

    // Get fault type
    let fault_type = obj
        .get(js_string!("fault"), context)
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .ok_or_else(|| anyhow!("Missing 'fault' field"))?;

    match fault_type.as_str() {
        "latency" => {
            let duration_ms = obj
                .get(js_string!("duration_ms"), context)
                .ok()
                .and_then(|v| v.as_number())
                .ok_or_else(|| anyhow!("Missing 'duration_ms' for latency fault"))?
                as u64;

            Ok(FaultDecision::Latency {
                duration_ms,
                rule_id: rule_id.to_string(),
            })
        }
        "error" => {
            let status = obj
                .get(js_string!("status"), context)
                .ok()
                .and_then(|v| v.as_number())
                .ok_or_else(|| anyhow!("Missing 'status' for error fault"))?
                as u16;

            let body = obj
                .get(js_string!("body"), context)
                .ok()
                .map(|v| {
                    if let Some(s) = v.as_string() {
                        s.to_std_string_escaped()
                    } else if v.is_object() {
                        // Convert object to JSON string
                        let json = js_to_json(context, &v).unwrap_or(Value::Null);
                        serde_json::to_string(&json).unwrap_or_else(|_| "{}".to_string())
                    } else {
                        v.display().to_string()
                    }
                })
                .unwrap_or_else(|| "{}".to_string());

            // Extract optional headers
            let mut headers = std::collections::HashMap::new();
            if let Ok(headers_val) = obj.get(js_string!("headers"), context)
                && let Some(headers_obj) = headers_val.as_object()
            {
                // Get all enumerable properties
                if let Ok(keys) = headers_obj.own_property_keys(context) {
                    for key in keys {
                        let key_str = match &key {
                            PropertyKey::String(s) => s.to_std_string_escaped(),
                            PropertyKey::Index(i) => i.get().to_string(),
                            PropertyKey::Symbol(_) => continue, // Skip symbols
                        };
                        if let Ok(val) = headers_obj.get(key.clone(), context) {
                            let val_str = if let Some(s) = val.as_string() {
                                s.to_std_string_escaped()
                            } else if let Some(n) = val.as_number() {
                                n.to_string()
                            } else if let Some(b) = val.as_boolean() {
                                b.to_string()
                            } else {
                                continue;
                            };
                            headers.insert(key_str, val_str);
                        }
                    }
                }
            }

            Ok(FaultDecision::Error {
                status,
                body,
                rule_id: rule_id.to_string(),
                headers,
            })
        }
        _ => Err(anyhow!("Unknown fault type: {fault_type}")),
    }
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
        return Ok(Value::Number(
            serde_json::Number::from_f64(n).unwrap_or(serde_json::Number::from(0)),
        ));
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

/// Execute a Mountebank-style inject function
///
/// Mountebank inject functions have the signature:
/// - `function(request) { return response; }`
/// - `function(request, state) { return response; }`
/// - `function(request, state, logger, callback) { callback(response); }`
///
/// Where response is: `{ statusCode: number, headers: object?, body: string }`
pub fn execute_mountebank_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    imposter_port: u16,
) -> Result<MountebankInjectResponse> {
    let mut context = Context::default();

    // Create request object
    let request_obj = create_mountebank_request_object(&mut context, request)?;

    // Get current state for this imposter
    let state_map = get_imposter_state(imposter_port);
    let state_obj = json_to_js(&mut context, &Value::Object(state_map))?;

    // Set global variables
    let global = context.global_object();
    global
        .set(
            js_string!("__request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set request: {e}"))?;
    global
        .set(
            js_string!("__state"),
            state_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set state: {e}"))?;

    // Wrap the inject function to call it with our arguments
    // Support both sync and callback styles
    // For callback style, we capture the result in __callbackResult
    let wrapper_script = format!(
        r#"
        var __injectFn = {inject_fn};
        var __callbackResult = null;
        var __logger = function() {{}};  // No-op logger
        var __callback = function(r) {{ __callbackResult = r; }};
        var __directResult;

        if (__injectFn.length >= 4) {{
            // Callback style: function(request, state, logger, callback)
            __injectFn(__request, __state, __logger, __callback);
            __directResult = __callbackResult;
        }} else {{
            // Sync style: function(request) or function(request, state)
            __directResult = __injectFn(__request, __state, __logger);
        }}
        __directResult;
        "#
    );

    // Execute the script
    let result = context
        .eval(Source::from_bytes(wrapper_script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute inject function: {e}"))?;

    // Save updated state back
    let updated_state = global
        .get(js_string!("__state"), &mut context)
        .map_err(|e| anyhow!("Failed to get updated state: {e}"))?;

    if let Ok(Value::Object(map)) = js_to_json(&mut context, &updated_state) {
        save_imposter_state(imposter_port, map);
    }

    // Parse the response
    parse_mountebank_response(&mut context, result)
}

/// Execute a Mountebank-style predicate inject function.
/// The function signature is: `function(request, logger, imposterState) { return bool; }`
/// Returns `true` if the predicate matches, `false` otherwise.
pub fn execute_predicate_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    imposter_port: u16,
) -> bool {
    let mut context = Context::default();

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

    let global = context.global_object();
    let _ = global.set(js_string!("__request"), request_obj, false, &mut context);
    let _ = global.set(js_string!("__state"), state_obj, false, &mut context);

    let wrapper_script = format!(
        r#"
        var __injectFn = {inject_fn};
        var __logger = {{ debug: function() {{}}, info: function() {{}}, warn: function() {{}}, error: function() {{}} }};
        var __result = __injectFn(__request, __logger, __state);
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

    // Update state
    if let Ok(updated_state) = global.get(js_string!("__state"), &mut context)
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
    let mut context = Context::default();

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

    let global = context.global_object();
    let _ = global.set(js_string!("__request"), request_obj, false, &mut context);
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
        var __logger = {{ debug: function() {{}}, info: function() {{}}, warn: function() {{}}, error: function(){{}} }};
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

/// Execute a Mountebank-style decorate behavior function
/// Format: function(request, response) { ... modifies response ... }
pub fn execute_mountebank_decorate(
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: u16,
    response_headers: &std::collections::HashMap<String, String>,
) -> Result<MountebankDecorateResponse> {
    let mut context = Context::default();

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

    // Set global variables
    let global = context.global_object();
    global
        .set(
            js_string!("__request"),
            request_obj.clone(),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set request: {e}"))?;
    global
        .set(
            js_string!("__response"),
            JsValue::from(response_obj),
            false,
            &mut context,
        )
        .map_err(|e| anyhow!("Failed to set response: {e}"))?;

    // Wrap the decorate function to call it with our arguments.
    // Provide a no-op logger (Mountebank's 3rd arg) and empty state (4th arg) so
    // scripts that reference logger.info(...) or state.counter don't throw ReferenceError.
    let wrapper_script = format!(
        r#"
        var __decorateFn = {decorate_fn};
        var __logger = {{ debug: function() {{}}, info: function() {{}}, warn: function() {{}}, error: function() {{}} }};
        var __state = {{}};
        __decorateFn(__request, __response, __logger, __state);
        __response;
        "#
    );

    // Execute the script
    let result = context
        .eval(Source::from_bytes(wrapper_script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute decorate function: {e}"))?;

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
function should_inject(request, flow_store) {
    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_js_engine_missing_function() {
        let script = r#"
function some_other_function() {
    return true;
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(engine.is_err());
        assert!(engine.unwrap_err().to_string().contains("should_inject"));
    }

    #[test]
    fn test_js_simple_fault_injection() {
        let script = r#"
function should_inject(request, flow_store) {
    if (request.path === "/api/test") {
        return {
            inject: true,
            fault: "error",
            status: 503,
            body: "Service unavailable"
        };
    }
    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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

    #[test]
    fn test_js_latency_fault() {
        let script = r#"
function should_inject(request, flow_store) {
    return {
        inject: true,
        fault: "latency",
        duration_ms: 1000
    };
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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

    #[test]
    fn test_js_flow_store_increment() {
        let script = r#"
function should_inject(request, flow_store) {
    var flow_id = request.headers["x-flow-id"] || "";
    if (flow_id === "") {
        return {inject: false};
    }

    var attempts = flow_store.increment(flow_id, "attempts");

    if (attempts <= 2) {
        return {
            inject: true,
            fault: "error",
            status: 503,
            body: "Attempt " + attempts
        };
    }

    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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

    #[test]
    fn test_js_flow_store_get_set() {
        let script = r#"
function should_inject(request, flow_store) {
    var flow_id = request.headers["x-flow-id"] || "";
    if (flow_id === "") {
        return {inject: false};
    }

    // Set a value
    flow_store.set(flow_id, "test_key", "test_value");

    // Get it back
    var value = flow_store.get(flow_id, "test_key");

    // Check if it matches
    if (value === "test_value") {
        return {
            inject: true,
            fault: "error",
            status: 200,
            body: "Get/Set works!"
        };
    }

    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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

    #[test]
    fn test_compile_js_to_bytecode() {
        let script = r#"
function should_inject(request, flow_store) {
    if (request.path === "/test") {
        return {
            inject: true,
            fault: "error",
            status: 500,
            body: "Test error"
        };
    }
    return {inject: false};
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
function should_inject(request, flow_store) {
    if (request.path === "/api/bytecode") {
        return {
            inject: true,
            fault: "error",
            status: 503,
            body: "Bytecode executed"
        };
    }
    return {inject: false};
}
"#;

        // Compile to bytecode
        let bytecode = compile_js_to_bytecode(script).unwrap();

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
function should_inject(request, flow_store) {
    if (request.body && request.body.nested && request.body.nested.value > 100) {
        return {
            inject: true,
            fault: "error",
            status: 400,
            body: "Value too high: " + request.body.nested.value
        };
    }
    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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
function should_inject(request, flow_store) {
    return {
        inject: true,
        fault: "error",
        status: 502,
        body: "Gateway error",
        headers: {
            "X-Custom-Header": "custom-value",
            "X-Error-Code": "E001"
        }
    };
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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
function should_inject(request, flow_store) {
    var name = request.query["name"];
    var page = request.query["page"];

    if (name && page) {
        return {
            inject: true,
            fault: "error",
            status: 200,
            body: "Hello " + name + " on page " + page
        };
    }
    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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

    #[test]
    fn test_js_path_params() {
        let script = r#"
function should_inject(request, flow_store) {
    var user_id = request.pathParams["id"];
    var action = request.pathParams["action"];

    if (user_id && action) {
        return {
            inject: true,
            fault: "error",
            status: 200,
            body: "User " + user_id + " action: " + action
        };
    }
    return {inject: false};
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
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
        let result = execute_mountebank_decorate(script, &request, "original", 200, &headers);
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
        let result = execute_mountebank_decorate(script, &request, "original", 200, &headers);
        assert!(
            result.is_ok(),
            "state arg should not throw: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().body, "state ok");
    }
}
