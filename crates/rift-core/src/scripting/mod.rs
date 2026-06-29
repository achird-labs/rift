use crate::extensions::flow_state::FlowStore;
use anyhow::{anyhow, Result};
use rhai::Dynamic;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// Engine modules (only used by proxy.rs for compilation)
mod rhai_engine;
pub use rhai_engine::RhaiEngine;

// Script pool for optimized execution
mod script_pool;
pub use script_pool::{CompiledScript, ScriptPool, ScriptPoolConfig};

// Decision cache for memoization
mod decision_cache;
pub use decision_cache::{CacheKey, DecisionCache, DecisionCacheConfig};

#[cfg(feature = "lua")]
mod lua_engine;
#[cfg(feature = "lua")]
pub use lua_engine::{compile_to_bytecode, LuaEngine};

#[cfg(feature = "javascript")]
mod js_engine;
#[cfg(feature = "javascript")]
pub use js_engine::{
    clear_imposter_state, compile_js_to_bytecode, execute_mountebank_decorate,
    execute_mountebank_inject, execute_predicate_generator_inject, execute_predicate_inject,
    JsEngine, MountebankRequest,
};
#[cfg(feature = "javascript")]
#[allow(unused_imports)]
pub use js_engine::{execute_js_bytecode, MountebankDecorateResponse, MountebankInjectResponse};

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

#[cfg(feature = "lua")]
mod lua_validator;
#[cfg(feature = "lua")]
#[allow(unused_imports)]
pub use lua_validator::LuaValidationError;
#[cfg(feature = "lua")]
pub use lua_validator::LuaValidator;

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
}

/// Unified script engine that supports Rhai, Lua, and JavaScript
#[derive(Clone)]

pub enum ScriptEngine {
    Rhai(RhaiEngine),
    #[cfg(feature = "lua")]
    Lua(LuaEngine),
    #[cfg(feature = "javascript")]
    JavaScript(JsEngine),
}

impl ScriptEngine {
    /// Create a new script engine based on the engine type
    pub fn new(engine_type: &str, script: &str, rule_id: String) -> Result<Self> {
        match engine_type {
            "rhai" => Ok(ScriptEngine::Rhai(RhaiEngine::new(script, rule_id)?)),
            #[cfg(feature = "lua")]
            "lua" => Ok(ScriptEngine::Lua(LuaEngine::new(script, rule_id)?)),
            #[cfg(not(feature = "lua"))]
            "lua" => Err(anyhow!(
                "Lua engine is not enabled. Enable the 'lua' feature flag"
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

    /// Execute the script and determine if a fault should be injected
    pub fn should_inject_fault(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        match self {
            ScriptEngine::Rhai(engine) => engine.should_inject_fault(request, flow_store),
            #[cfg(feature = "lua")]
            ScriptEngine::Lua(engine) => engine.should_inject(request, flow_store),
            #[cfg(feature = "javascript")]
            ScriptEngine::JavaScript(engine) => engine.should_inject(request, flow_store),
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
}

/// Wrapper for FlowStore that can be used in scripts (both Rhai and Lua)
/// Uses direct synchronous calls since FlowStore is no longer async
#[derive(Clone)]

pub struct ScriptFlowStore {
    store: Arc<dyn FlowStore>,
}

impl ScriptFlowStore {
    pub fn new(store: Arc<dyn FlowStore>) -> Self {
        Self { store }
    }

    /// Get a value from flow state
    pub fn get(&mut self, flow_id: String, key: String) -> Dynamic {
        match self.store.get(&flow_id, &key) {
            Ok(Some(val)) => rhai_engine::json_to_dynamic(val),
            _ => Dynamic::UNIT,
        }
    }

    /// Set a value in flow state
    pub fn set(&mut self, flow_id: String, key: String, value: Dynamic) -> bool {
        let json_val = rhai_engine::dynamic_to_json(value);
        self.store.set(&flow_id, &key, json_val).is_ok()
    }

    /// Check if a key exists
    pub fn exists(&mut self, flow_id: String, key: String) -> bool {
        self.store.exists(&flow_id, &key).unwrap_or(false)
    }

    /// Delete a key
    pub fn delete(&mut self, flow_id: String, key: String) -> bool {
        self.store.delete(&flow_id, &key).is_ok()
    }

    /// Increment a counter
    pub fn increment(&mut self, flow_id: String, key: String) -> i64 {
        self.store.increment(&flow_id, &key).unwrap_or(0)
    }

    /// Set TTL for a flow
    pub fn set_ttl(&mut self, flow_id: String, ttl_seconds: i64) -> bool {
        self.store.set_ttl(&flow_id, ttl_seconds).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::flow_state::NoOpFlowStore;
    use std::collections::HashMap;

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

    // ============================================
    // Tests for ScriptRequest
    // ============================================

    #[test]
    fn test_script_request_creation() {
        let request = ScriptRequest {
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
        let result = script_store.get("flow-1".to_string(), "key".to_string());
        // NoOpFlowStore returns Unit for get
        assert!(result.is_unit());
    }

    #[test]
    fn test_script_flow_store_set() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store.set(
            "flow-1".to_string(),
            "key".to_string(),
            rhai::Dynamic::from(42),
        );
        assert!(result);
    }

    #[test]
    fn test_script_flow_store_exists() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store.exists("flow-1".to_string(), "key".to_string());
        assert!(!result); // NoOpFlowStore always returns false
    }

    #[test]
    fn test_script_flow_store_delete() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store.delete("flow-1".to_string(), "key".to_string());
        assert!(result);
    }

    #[test]
    fn test_script_flow_store_increment() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        // NoOpFlowStore returns 0 on error (which doesn't happen, but increment returns 1)
        let result = script_store.increment("flow-1".to_string(), "counter".to_string());
        assert_eq!(result, 1);
    }

    #[test]
    fn test_script_flow_store_set_ttl() {
        let store = Arc::new(NoOpFlowStore);
        let mut script_store = ScriptFlowStore::new(store);
        let result = script_store.set_ttl("flow-1".to_string(), 3600);
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
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = ScriptEngine::new("rhai", script, "test-rule".to_string());
        assert!(engine.is_ok());
    }

    #[test]
    fn test_script_engine_new_invalid_type() {
        let script = "return false";
        let engine = ScriptEngine::new("invalid_engine", script, "test-rule".to_string());
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("Unknown script engine type"));
    }

    #[test]
    fn test_script_engine_rhai_execution() {
        let script = r#"
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = ScriptEngine::new("rhai", script, "test-rule".to_string()).unwrap();

        let request = ScriptRequest {
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
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = RhaiEngine::new(script, "valid-rule".to_string());
        assert!(engine.is_ok());
    }

    #[test]
    fn test_rhai_engine_creation_syntax_error() {
        let script = r#"
            fn should_inject(request {  // Missing closing paren
                return false;
            }
        "#;
        let engine = RhaiEngine::new(script, "invalid-rule".to_string());
        assert!(engine.is_err());
    }

    #[test]
    fn test_rhai_engine_ast_access() {
        let script = r#"
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = RhaiEngine::new(script, "test-rule".to_string()).unwrap();
        let ast = engine.ast();
        assert!(std::mem::size_of_val(ast) > 0);
    }

    // ============================================
    // Feature-gated tests
    // ============================================

    #[cfg(feature = "lua")]
    mod lua_tests {
        use super::*;

        #[test]
        fn test_script_engine_new_lua() {
            let script = r#"
                function should_inject(request, flow_store)
                    return { inject = false }
                end
            "#;
            let engine = ScriptEngine::new("lua", script, "lua-rule".to_string());
            assert!(
                engine.is_ok(),
                "Lua engine creation failed: {:?}",
                engine.err()
            );
        }

        #[test]
        fn test_compile_to_bytecode() {
            let script = r#"
                function should_inject(request, flow_store)
                    return { inject = false }
                end
            "#;
            let bytecode = super::super::compile_to_bytecode(script);
            assert!(bytecode.is_ok());
        }
    }

    #[cfg(feature = "javascript")]
    mod js_tests {
        use super::*;

        #[test]
        fn test_script_engine_new_javascript() {
            let script = r#"
                function should_inject(request, flow_store) {
                    return { inject: false };
                }
            "#;
            let engine = ScriptEngine::new("javascript", script, "js-rule".to_string());
            assert!(
                engine.is_ok(),
                "JS engine creation failed: {:?}",
                engine.err()
            );
        }

        #[test]
        fn test_script_engine_new_js_alias() {
            let script = r#"
                function should_inject(request, flow_store) {
                    return { inject: false };
                }
            "#;
            let engine = ScriptEngine::new("js", script, "js-rule".to_string());
            assert!(
                engine.is_ok(),
                "JS engine creation failed: {:?}",
                engine.err()
            );
        }

        #[test]
        fn test_compile_js_to_bytecode() {
            let script = r#"
                function should_inject(request, flow_store) {
                    return { inject: false };
                }
            "#;
            let bytecode = super::super::compile_js_to_bytecode(script);
            assert!(bytecode.is_ok());
        }
    }

    // ============================================
    // Tests for disabled features
    // ============================================

    #[cfg(not(feature = "lua"))]
    #[test]
    fn test_lua_engine_disabled() {
        let engine = ScriptEngine::new("lua", "return false", "test".to_string());
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("not enabled") || err_msg.contains("feature"));
    }

    #[cfg(not(feature = "javascript"))]
    #[test]
    fn test_javascript_engine_disabled() {
        let engine = ScriptEngine::new("javascript", "return false", "test".to_string());
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("not enabled") || err_msg.contains("feature"));
    }
}
