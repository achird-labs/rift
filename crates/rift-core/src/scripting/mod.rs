use crate::extensions::flow_state::{FlowStore, flow_result, strict_flow_store};
use anyhow::{Result, anyhow};
use rhai::Dynamic;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// Engine modules (only used by proxy.rs for compilation)
mod bounded;
mod compiled_cache;
mod rhai_engine;
pub use bounded::{DEFAULT_SCRIPT_TIMEOUT_MS, resolve_script_timeout_ms, should_inject_bounded};

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
pub use lua_engine::{LuaEngine, compile_to_bytecode};

#[cfg(feature = "javascript")]
mod js_engine;
/// Exposed so other modules (e.g. `behaviors::wait`) that run a standalone JS snippet outside the
/// MB inject/predicate/decorate hooks still get the same interpreter-level guards (issue #355
/// Items 3/6) rather than an unbounded `Context::default()`.
#[cfg(feature = "javascript")]
pub(crate) use js_engine::bounded_js_context;
#[cfg(feature = "javascript")]
pub use js_engine::{
    JsEngine, MountebankRequest, clear_imposter_state, compile_js_to_bytecode,
    execute_mountebank_config_decorate, execute_mountebank_decorate, execute_mountebank_inject,
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
    pub fn new(engine_type: &str, script: &str, rule_id: &str) -> Result<Self> {
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

    /// Get a value from flow state. In strict mode (issue #376) a backend failure raises; otherwise
    /// it returns unit (the lenient #322 fallback) and records the error for `last_error()`.
    pub fn get(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<Dynamic, Box<rhai::EvalAltResult>> {
        match flow_result("get", self.store.get(&flow_id, &key)) {
            Ok(Some(val)) => Ok(rhai_engine::json_to_dynamic(val)),
            Ok(None) => Ok(Dynamic::UNIT),
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(Dynamic::UNIT),
        }
    }

    /// Set a value in flow state. Strict mode raises on failure; else returns false (lenient #322).
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
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(false),
        }
    }

    /// Check if a key exists. Strict mode raises on failure; else returns false (lenient #322).
    pub fn exists(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        match flow_result("exists", self.store.exists(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(false),
        }
    }

    /// Delete a key. Strict mode raises on failure; else returns false (lenient #322).
    pub fn delete(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<bool, Box<rhai::EvalAltResult>> {
        match flow_result("delete", self.store.delete(&flow_id, &key).map(|()| true)) {
            Ok(v) => Ok(v),
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(false),
        }
    }

    /// Increment a counter. Strict mode raises on failure; else returns 0 (lenient #322).
    pub fn increment(
        &mut self,
        flow_id: String,
        key: String,
    ) -> std::result::Result<i64, Box<rhai::EvalAltResult>> {
        match flow_result("increment", self.store.increment(&flow_id, &key)) {
            Ok(v) => Ok(v),
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(0),
        }
    }

    /// Set TTL for a flow. Strict mode raises on failure; else returns false (lenient #322).
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
            Err(msg) if strict_flow_store() => Err(msg.into()),
            Err(_) => Ok(false),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::flow_state::NoOpFlowStore;
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
    }

    // Issue #376 lenient contract (default, strict OFF): every Rhai ScriptFlowStore op, on a backend
    // failure, returns its documented fallback (never raises) AND records the error for
    // `last_error()` (issue #322). Covers all six ops — the strict/lenient branch is hand-written
    // per op, so a wrong fallback in any one would be caught here. (The strict raise is covered
    // end-to-end per engine in issue_376_strict_flow_store.rs; it can't be unit-tested reliably
    // because `strict_flow_store()` caches the env read per process.)
    #[test]
    fn rhai_flow_store_lenient_ops_return_fallback_and_record_error() {
        use crate::extensions::flow_state::take_last_flow_error;
        let mut s = ScriptFlowStore::new(Arc::new(FailingStore));

        let _ = take_last_flow_error();
        assert!(
            s.get("f".into(), "k".into())
                .expect("lenient get must not raise")
                .is_unit(),
            "get falls back to unit"
        );
        assert!(
            take_last_flow_error().is_some_and(|e| e.contains("get")),
            "get records last_error"
        );

        assert!(
            !s.set("f".into(), "k".into(), Dynamic::from(1))
                .expect("lenient set must not raise"),
            "set falls back to false"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("set")));

        assert!(
            !s.exists("f".into(), "k".into())
                .expect("lenient exists must not raise"),
            "exists falls back to false"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("exists")));

        assert!(
            !s.delete("f".into(), "k".into())
                .expect("lenient delete must not raise"),
            "delete falls back to false"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("delete")));

        assert_eq!(
            s.increment("f".into(), "k".into())
                .expect("lenient increment must not raise"),
            0,
            "increment falls back to 0"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("increment")));

        assert!(
            !s.set_ttl("f".into(), 60)
                .expect("lenient set_ttl must not raise"),
            "set_ttl falls back to false"
        );
        assert!(take_last_flow_error().is_some_and(|e| e.contains("setTtl")));
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
            fn should_inject(request, flow_store) {
                #{ inject: false }
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
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = ScriptEngine::new("rhai", script, "test-rule").unwrap();

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
        let engine = RhaiEngine::new(script, "valid-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_rhai_engine_creation_syntax_error() {
        let script = r#"
            fn should_inject(request {  // Missing closing paren
                return false;
            }
        "#;
        let engine = RhaiEngine::new(script, "invalid-rule");
        assert!(engine.is_err());
    }

    #[test]
    fn test_rhai_engine_ast_access() {
        let script = r#"
            fn should_inject(request, flow_store) {
                #{ inject: false }
            }
        "#;
        let engine = RhaiEngine::new(script, "test-rule").unwrap();
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
            let engine = ScriptEngine::new("lua", script, "lua-rule");
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
                function should_inject(request, flow_store) {
                    return { inject: false };
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
        let engine = ScriptEngine::new("lua", "return false", "test");
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("not enabled") || err_msg.contains("feature"));
    }

    #[cfg(not(feature = "javascript"))]
    #[test]
    fn test_javascript_engine_disabled() {
        let engine = ScriptEngine::new("javascript", "return false", "test");
        assert!(engine.is_err());
        let err_msg = engine.err().unwrap().to_string();
        assert!(err_msg.contains("not enabled") || err_msg.contains("feature"));
    }
}
