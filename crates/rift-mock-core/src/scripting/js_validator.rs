use super::validator::{ScriptValidationError, ScriptValidator};
use boa_engine::{Context, Source};
use std::error::Error;
use std::fmt;

/// JavaScript script validation error.
#[derive(Debug, Clone)]
pub enum JsValidationError {
    /// Required function is missing from the script
    MissingFunction(String),
    /// Script failed to evaluate
    EvaluationError(String),
}

impl fmt::Display for JsValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsValidationError::MissingFunction(func) => {
                write!(f, "Missing required function: {func}")
            }
            JsValidationError::EvaluationError(msg) => write!(f, "Evaluation error: {msg}"),
        }
    }
}

impl Error for JsValidationError {}

impl From<JsValidationError> for ScriptValidationError {
    fn from(err: JsValidationError) -> Self {
        match err {
            JsValidationError::MissingFunction(func) => ScriptValidationError::MissingFunction {
                engine: "javascript".to_string(),
                function: func,
            },
            JsValidationError::EvaluationError(msg) => ScriptValidationError::SyntaxError {
                engine: "javascript".to_string(),
                message: msg,
            },
        }
    }
}

/// Validator for JavaScript scripts.
///
/// This validator checks that JavaScript scripts are syntactically valid.
/// It does not require any particular function to be defined (issue #453 removed the
/// old requirement that a script define `should_inject`, the v1 wrapper).
pub struct JsValidator;

impl JsValidator {
    /// Creates a new JavaScript validator.
    pub fn new() -> Self {
        Self
    }

    /// Validate JavaScript script syntax (static method for backwards compatibility).
    ///
    /// This is a convenience method that creates a temporary validator instance.
    pub fn validate_static(script: &str) -> Result<(), JsValidationError> {
        Self::new().validate(script)
    }
}

impl Default for JsValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptValidator for JsValidator {
    type Error = JsValidationError;

    /// Validates a JavaScript script for use with Rift proxy.
    ///
    /// # Checks performed
    /// 1. Script parses without errors
    ///
    /// Validation is syntax-only, matching `JsEngine::new`: a v2 bare-expression script
    /// (e.g. `http(503, "boom")`) references globals (`ctx`, `http`, ...) that are only bound
    /// at real execution time, so fully evaluating it here would spuriously fail with an
    /// unbound-identifier error. `Script::parse` catches genuine syntax errors without
    /// executing anything.
    fn validate(&self, script: &str) -> Result<(), Self::Error> {
        let mut context = Context::default();
        boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut context)
            .map_err(|e| JsValidationError::EvaluationError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_script() {
        let validator = JsValidator::new();
        let script = r#"
function should_inject(request, flow_store) {
    return {inject: false};
}
"#;
        assert!(validator.validate(script).is_ok());
    }

    #[test]
    fn test_syntax_error() {
        let validator = JsValidator::new();
        let script = r#"
function should_inject(request, flow_store {
    return {inject: false};
}
"#;
        let result = validator.validate(script);
        assert!(result.is_err());
        assert!(matches!(result, Err(JsValidationError::EvaluationError(_))));
    }

    #[test]
    fn test_v2_script_without_should_inject_is_valid() {
        // Issue #453: validation is syntax-only now — a script that never defines
        // `should_inject` (the removed v1 wrapper), including a v2 bare-expression script
        // referencing globals (`http`) only bound at real execution time, must still validate
        // fine since we only parse, never evaluate.
        let validator = JsValidator::new();
        let script = r#"http(503, "boom");"#;
        let result = validator.validate(script);
        assert!(result.is_ok(), "v2 bare script should validate: {result:?}");
    }

    #[test]
    fn test_complex_script() {
        let validator = JsValidator::new();
        let script = r#"
function should_inject(request, flow_store) {
    var flowId = request.headers["x-flow-id"];
    if (!flowId) {
        return {inject: false};
    }

    var attempts = flow_store.increment(flowId, "attempts");

    if (attempts <= 2) {
        return {
            inject: true,
            fault: "error",
            status: 503,
            body: "Service temporarily unavailable"
        };
    }

    return {inject: false};
}
"#;
        assert!(validator.validate(script).is_ok());
    }

    #[test]
    fn test_batch_validation() {
        let validator = JsValidator::new();
        let scripts = vec![
            (
                "script1",
                r#"function should_inject(r, f) { return {inject: false}; }"#,
            ),
            // Validation is syntax-only (issue #453) — a genuine syntax error, not a
            // wrongly-named function, is what makes script2 invalid.
            ("script2", r#"function other_func(r, f) { return {inject: "#),
            (
                "script3",
                r#"function should_inject(r, f) { return {inject: true}; }"#,
            ),
        ];

        let results = validator.validate_batch(&scripts);

        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_ok(), "script1 should be valid");
        assert!(results[1].1.is_err(), "script2 should be invalid");
        assert!(results[2].1.is_ok(), "script3 should be valid");
    }

    #[test]
    fn test_static_validate() {
        let script = r#"
function should_inject(request, flow_store) {
    return {inject: false};
}
"#;
        // Test the static method still works for backwards compatibility
        assert!(JsValidator::validate_static(script).is_ok());
    }

    #[test]
    fn test_error_conversion() {
        let js_err = JsValidationError::MissingFunction("should_inject".to_string());
        let unified_err: ScriptValidationError = js_err.into();

        assert!(matches!(
            unified_err,
            ScriptValidationError::MissingFunction { engine, .. } if engine == "javascript"
        ));
    }
}
