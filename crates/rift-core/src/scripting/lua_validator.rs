use super::validator::{ScriptValidationError, ScriptValidator};
use mlua::Lua;
use std::error::Error;
use std::fmt;

/// Lua script validation error.
#[derive(Debug, Clone)]
pub enum LuaValidationError {
    /// Script contains syntax errors
    SyntaxError(String),
    /// Script is missing a return statement
    MissingReturnStatement(String),
    /// Script failed to compile
    CompilationError(String),
    /// Script failed to load
    LoadError(String),
}

impl fmt::Display for LuaValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LuaValidationError::SyntaxError(msg) => write!(f, "Syntax error: {msg}"),
            LuaValidationError::MissingReturnStatement(msg) => {
                write!(f, "Missing return statement: {msg}")
            }
            LuaValidationError::CompilationError(msg) => write!(f, "Compilation error: {msg}"),
            LuaValidationError::LoadError(msg) => write!(f, "Load error: {msg}"),
        }
    }
}

impl Error for LuaValidationError {}

impl From<LuaValidationError> for ScriptValidationError {
    fn from(err: LuaValidationError) -> Self {
        match err {
            LuaValidationError::SyntaxError(msg) => ScriptValidationError::SyntaxError {
                engine: "lua".to_string(),
                message: msg,
            },
            LuaValidationError::MissingReturnStatement(msg) => ScriptValidationError::SyntaxError {
                engine: "lua".to_string(),
                message: format!("Missing return statement: {msg}"),
            },
            LuaValidationError::CompilationError(msg) => ScriptValidationError::CompilationError {
                engine: "lua".to_string(),
                message: msg,
            },
            LuaValidationError::LoadError(msg) => ScriptValidationError::LoadError {
                engine: "lua".to_string(),
                message: msg,
            },
        }
    }
}

/// Validator for Lua scripts.
pub struct LuaValidator {
    lua: Lua,
}

impl LuaValidator {
    /// Creates a new Lua validator.
    pub fn new() -> Self {
        Self { lua: Lua::new() }
    }
}

impl Default for LuaValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptValidator for LuaValidator {
    type Error = LuaValidationError;

    /// Validates a Lua script for use with Rift proxy.
    ///
    /// # Checks performed
    /// 1. Script compiles without syntax errors
    /// 2. Script can be loaded as a chunk
    ///
    /// Note: This validates syntax only - runtime behavior depends on request/flow_store context.
    fn validate(&self, script: &str) -> Result<(), Self::Error> {
        // Try to compile (load and parse) - this catches syntax errors
        match self.lua.load(script).eval::<mlua::Value>() {
            Ok(_) => {
                // Script loaded and executed successfully
                Ok(())
            }
            Err(e) => {
                // Check if it's a syntax error vs runtime error
                let err_str = e.to_string();
                if err_str.contains("syntax error")
                    || err_str.contains("unexpected")
                    || err_str.contains("'end' expected")
                {
                    Err(LuaValidationError::SyntaxError(err_str))
                } else {
                    // Runtime errors during validation are okay (e.g., undefined variables like request)
                    // We only care about syntax validation
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_script() {
        let validator = LuaValidator::new();
        let script = r#"
            local flow_id = request.headers["x-flow-id"]
            if flow_id == nil then
                return { inject = false }
            end

            local count = flow_store:increment(flow_id, "count")
            if count > 5 then
                return {
                    inject = true,
                    fault = "error",
                    status = 503,
                    body = "Too many requests"
                }
            end

            return { inject = false }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "Valid script should pass validation");
    }

    #[test]
    fn test_syntax_error() {
        let validator = LuaValidator::new();
        let script = r#"
            local flow_id = request.headers["x-flow-id"
            -- Missing closing bracket
            return { inject = false }
        "#;

        let result = validator.validate(script);
        assert!(result.is_err());
        assert!(matches!(result, Err(LuaValidationError::SyntaxError(_))));
    }

    #[test]
    fn test_complex_valid_script() {
        let validator = LuaValidator::new();
        let script = r#"
            -- Circuit breaker pattern
            local flow_id = request.headers["x-flow-id"]
            if flow_id == nil then
                return { inject = false }
            end

            local failures = flow_store:increment(flow_id, "failures")
            flow_store:set_ttl(flow_id, 300)

            if failures > 3 then
                return {
                    inject = true,
                    fault = "error",
                    status = 503,
                    body = "Circuit breaker open"
                }
            end

            return { inject = false }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "Complex valid script should pass");
    }

    #[test]
    fn test_batch_validation() {
        let validator = LuaValidator::new();
        let scripts = vec![
            ("script1", r#"return { inject = false }"#),
            ("script2", r#"return { inject = true "#), // Missing closing brace
            (
                "script3",
                r#"local x = flow_store:increment("flow1", "key") return { inject = false }"#,
            ),
        ];

        let results = validator.validate_batch(&scripts);

        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_ok(), "script1 should be valid");
        assert!(results[1].1.is_err(), "script2 should be invalid");
        assert!(results[2].1.is_ok(), "script3 should be valid");
    }

    #[test]
    fn test_latency_fault_script() {
        let validator = LuaValidator::new();
        let script = r#"
            if request.path:find("/slow") then
                return {
                    inject = true,
                    fault = "latency",
                    duration_ms = 1000
                }
            end
            return { inject = false }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "Latency fault script should be valid");
    }

    #[test]
    fn test_error_conversion() {
        let lua_err = LuaValidationError::SyntaxError("unexpected token".to_string());
        let unified_err: ScriptValidationError = lua_err.into();

        assert!(matches!(
            unified_err,
            ScriptValidationError::SyntaxError { engine, .. } if engine == "lua"
        ));
    }
}
