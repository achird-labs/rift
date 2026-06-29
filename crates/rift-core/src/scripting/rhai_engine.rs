use crate::extensions::flow_state::FlowStore;
use anyhow::{anyhow, Result};
use rhai::{Dynamic, Engine, Map, Scope, AST};
use serde_json::Value;
use std::sync::Arc;

use super::{FaultDecision, ScriptFlowStore, ScriptRequest};

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
    pub fn new(script: &str, rule_id: String) -> Result<Self> {
        let engine = Self::create_engine();
        let ast = engine
            .compile(script)
            .map_err(|e| anyhow!("Failed to compile script: {e}"))?;

        Ok(Self {
            ast: Arc::new(ast), // Wrap AST in Arc for sharing
            rule_id,
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
            .register_fn("set_ttl", ScriptFlowStore::set_ttl);

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

        engine
    }

    pub fn should_inject_fault(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        let engine = Self::create_engine();
        let mut scope = Scope::new();

        // Create request map
        let request_map = create_request_map(request);

        // Create flow_store wrapper
        let flow_store_wrapper = ScriptFlowStore::new(flow_store);

        // First, evaluate the AST to define the should_inject function
        engine
            .run_ast_with_scope(&mut scope, self.ast.as_ref())
            .map_err(|e| anyhow!("Script execution error: {e}"))?;

        // Now call the should_inject function with request and flow_store arguments
        let result: Dynamic = engine
            .call_fn(
                &mut scope,
                self.ast.as_ref(),
                "should_inject",
                (request_map, flow_store_wrapper),
            )
            .map_err(|e| anyhow!("Failed to call should_inject function: {e}"))?;

        // Parse result
        self.parse_fault_decision(result)
    }

    fn parse_fault_decision(&self, result: Dynamic) -> Result<FaultDecision> {
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
                    rule_id: self.rule_id.clone(),
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
                        } else if let Some(m) = v.clone().try_cast::<Map>() {
                            // Convert map to JSON string
                            serde_json::to_string(&dynamic_to_json(Dynamic::from(m)))
                                .unwrap_or_else(|_| "{}".to_string())
                        } else {
                            format!("{v}")
                        }
                    })
                    .unwrap_or_else(|| "{}".to_string());

                // Extract optional headers map
                let mut headers = std::collections::HashMap::new();
                if let Some(headers_value) = map.get("headers") {
                    if let Some(headers_map) = headers_value.clone().try_cast::<Map>() {
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
                }

                Ok(FaultDecision::Error {
                    status: status as u16,
                    body,
                    rule_id: self.rule_id.clone(),
                    headers,
                })
            }
            _ => Err(anyhow!("Unknown fault type: {fault_type}")),
        }
    }
}

/// Public function to execute Rhai script with a reusable engine (for script pool)
/// This is used by the script_pool module to execute scripts efficiently
pub fn execute_rhai_with_engine(
    engine: &Engine,
    ast: &Arc<AST>,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let mut scope = Scope::new();

    // Create request map
    let request_map = create_request_map(request);

    // Create flow_store wrapper
    let flow_store_wrapper = ScriptFlowStore::new(flow_store);

    // First, evaluate the AST to define the should_inject function
    engine
        .run_ast_with_scope(&mut scope, ast)
        .map_err(|e| anyhow!("Script execution error: {e}"))?;

    // Now call the should_inject function with request and flow_store arguments
    let result: Dynamic = engine
        .call_fn(
            &mut scope,
            ast,
            "should_inject",
            (request_map, flow_store_wrapper),
        )
        .map_err(|e| anyhow!("Failed to call should_inject function: {e}"))?;

    // Parse result
    parse_fault_decision_with_rule_id(result, rule_id)
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
                    } else if let Some(m) = v.clone().try_cast::<Map>() {
                        // Convert map to JSON string
                        serde_json::to_string(&dynamic_to_json(Dynamic::from(m)))
                            .unwrap_or_else(|_| "{}".to_string())
                    } else {
                        format!("{v}")
                    }
                })
                .unwrap_or_else(|| "{}".to_string());

            // Extract optional headers map
            let mut headers = std::collections::HashMap::new();
            if let Some(headers_value) = map.get("headers") {
                if let Some(headers_map) = headers_value.clone().try_cast::<Map>() {
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
    } else if let Some(arr) = value.clone().try_cast::<Vec<Dynamic>>() {
        Value::Array(arr.into_iter().map(dynamic_to_json).collect())
    } else if let Some(map) = value.clone().try_cast::<Map>() {
        let mut obj = serde_json::Map::new();
        for (k, v) in map {
            obj.insert(k.to_string(), dynamic_to_json(v));
        }
        Value::Object(obj)
    } else {
        Value::String(format!("{value}"))
    }
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

        let engine = RhaiEngine::new(script, "test-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
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

        let engine = RhaiEngine::new(script, "latency-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
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

        let engine = RhaiEngine::new(script, "retry-rule".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = HashMap::new();
        headers.insert("x-flow-id".to_string(), "flow123".to_string());

        let request = ScriptRequest {
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

        let engine = RhaiEngine::new(script, "beta-users".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Beta user - should inject
        let mut headers1 = HashMap::new();
        headers1.insert("x-user-id".to_string(), "beta-user-123".to_string());

        let request1 = ScriptRequest {
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

        let engine = RhaiEngine::new(script, "cache-test".to_string()).unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Get AST reference (Arc) - this is what script pool will use
        let ast = engine.ast();

        // Create a reusable engine (simulating script pool worker)
        let reusable_engine = RhaiEngine::create_engine();

        // Execute same AST multiple times with reusable engine
        for i in 0..10 {
            let request = ScriptRequest {
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
}
