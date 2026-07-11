use super::validator::{ScriptValidationError, ScriptValidator};
use rhai::{AST, Engine};
use std::error::Error;
use std::fmt;

/// Rhai script validation error.
#[derive(Debug, Clone)]
pub enum RhaiValidationError {
    /// Script contains syntax errors
    SyntaxError(String),
    /// Required function is missing from the script
    MissingFunction(String),
    /// Function signature is invalid
    InvalidSignature(String),
    /// Script failed to compile
    CompilationError(String),
}

impl fmt::Display for RhaiValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RhaiValidationError::SyntaxError(msg) => write!(f, "Syntax error: {msg}"),
            RhaiValidationError::MissingFunction(func) => {
                write!(f, "Missing required function: {func}")
            }
            RhaiValidationError::InvalidSignature(msg) => {
                write!(f, "Invalid function signature: {msg}")
            }
            RhaiValidationError::CompilationError(msg) => write!(f, "Compilation error: {msg}"),
        }
    }
}

impl Error for RhaiValidationError {}

impl From<RhaiValidationError> for ScriptValidationError {
    fn from(err: RhaiValidationError) -> Self {
        match err {
            RhaiValidationError::SyntaxError(msg) => ScriptValidationError::SyntaxError {
                engine: "rhai".to_string(),
                message: msg,
            },
            RhaiValidationError::MissingFunction(func) => ScriptValidationError::MissingFunction {
                engine: "rhai".to_string(),
                function: func,
            },
            RhaiValidationError::InvalidSignature(msg) => ScriptValidationError::CompilationError {
                engine: "rhai".to_string(),
                message: msg,
            },
            RhaiValidationError::CompilationError(msg) => ScriptValidationError::CompilationError {
                engine: "rhai".to_string(),
                message: msg,
            },
        }
    }
}

/// Validator for Rhai scripts.
pub struct RhaiValidator {
    engine: Engine,
}

impl RhaiValidator {
    /// Creates a new Rhai validator.
    pub fn new() -> Self {
        let engine = Engine::new();
        Self { engine }
    }

    /// Validates a Rhai script and returns the compiled AST on success.
    ///
    /// This method is useful when you need both validation and the AST
    /// for subsequent operations.
    ///
    /// # Checks performed
    /// 1. Script compiles without syntax errors
    ///
    /// Note: This does NOT validate runtime behavior - only syntax and structure. Issue #453
    /// removed the old requirement that a script define `should_inject` (the v1 wrapper); v2
    /// scripts define `respond(ctx)` or are bare expressions with no function definitions at
    /// all, so validity is syntax-only here, matching `RhaiEngine::new`.
    pub fn validate_with_ast(&self, script: &str) -> Result<AST, RhaiValidationError> {
        // Compile the script - this catches syntax errors
        let ast = self
            .engine
            .compile(script)
            .map_err(|e| RhaiValidationError::SyntaxError(e.to_string()))?;

        Ok(ast)
    }
}

impl Default for RhaiValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptValidator for RhaiValidator {
    type Error = RhaiValidationError;

    fn validate(&self, script: &str) -> Result<(), Self::Error> {
        self.validate_with_ast(script).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_script() {
        let validator = RhaiValidator::new();
        let script = r#"
            fn should_inject(request, flow_store) {
                return #{ inject: true, fault: "latency", duration_ms: 100 };
            }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "Valid script should pass validation");
    }

    #[test]
    fn test_syntax_error() {
        let validator = RhaiValidator::new();
        let script = r#"
            fn should_inject(request, flow_store) {
                return #{ inject: true  // Missing closing brace
            }
        "#;

        let result = validator.validate(script);
        assert!(result.is_err());
        assert!(matches!(result, Err(RhaiValidationError::SyntaxError(_))));
    }

    #[test]
    fn test_v2_script_without_should_inject_is_valid() {
        // Issue #453: validation is syntax-only now — a script that never defines
        // `should_inject` (the removed v1 wrapper) must still validate fine.
        let validator = RhaiValidator::new();
        let script = r#"
            fn respond(ctx) {
                http(200, "ok")
            }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "v2 script without should_inject is valid");
    }

    #[test]
    fn test_complex_valid_script() {
        let validator = RhaiValidator::new();
        let script = r#"
            fn should_inject(request, flow_store) {
                let path = request.path;
                if path.contains("/api/") {
                    let flow_id = request.headers["x-flow-id"];
                    let attempts = flow_store.increment(flow_id, "attempts");

                    if attempts <= 2 {
                        return #{ inject: true, fault: "error", status: 503 };
                    }
                }
                return #{ inject: false };
            }
        "#;

        let result = validator.validate(script);
        assert!(result.is_ok(), "Complex valid script should pass");
    }

    #[test]
    fn test_batch_validation() {
        let validator = RhaiValidator::new();
        let scripts = vec![
            (
                "script1",
                r#"fn should_inject(req, fs) { return #{ inject: false }; }"#,
            ),
            // Validation is syntax-only (issue #453) — a genuine syntax error, not a
            // wrongly-named function, is what makes script2 invalid.
            ("script2", r#"fn wrong_name() { return true; "#),
            (
                "script3",
                r#"fn should_inject(req, fs) { return #{ inject: true, fault: "latency", duration_ms: 50 }; }"#,
            ),
        ];

        let results = validator.validate_batch(&scripts);

        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_ok(), "script1 should be valid");
        assert!(results[1].1.is_err(), "script2 should be invalid");
        assert!(results[2].1.is_ok(), "script3 should be valid");
    }

    #[test]
    fn test_validate_with_ast() {
        let validator = RhaiValidator::new();
        let script = r#"
            fn should_inject(request, flow_store) {
                return #{ inject: false };
            }
        "#;

        let result = validator.validate_with_ast(script);
        assert!(result.is_ok(), "Should return AST for valid script");
        assert!(result.unwrap().source().is_none()); // AST exists but has no source name
    }

    #[test]
    fn test_error_conversion() {
        let rhai_err = RhaiValidationError::SyntaxError("test error".to_string());
        let unified_err: ScriptValidationError = rhai_err.into();

        assert!(matches!(
            unified_err,
            ScriptValidationError::SyntaxError { engine, .. } if engine == "rhai"
        ));
    }
}
