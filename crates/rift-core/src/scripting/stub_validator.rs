//! Stub script validation for Admin API.
//!
//! Validates scripts in stub responses before they are added to imposters,
//! ensuring syntax errors and missing functions are caught at configuration time
//! rather than at request time.

use super::validator::ScriptValidator;
use crate::imposter::{RiftScriptConfig, Stub, StubResponse};
use std::fmt;

/// Error type for stub script validation
#[derive(Debug, Clone)]
pub struct StubValidationError {
    /// Stub identifier (id or index)
    pub stub_id: String,
    /// Response index within the stub
    pub response_index: usize,
    /// Script engine type
    pub engine: String,
    /// Detailed error message
    pub message: String,
}

impl fmt::Display for StubValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Invalid {} script in stub '{}' response {}: {}",
            self.engine, self.stub_id, self.response_index, self.message
        )
    }
}

impl std::error::Error for StubValidationError {}

/// Result of validating stubs
#[derive(Debug)]
pub struct StubValidationResult {
    pub errors: Vec<StubValidationError>,
}

impl StubValidationResult {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn into_error_message(self) -> Option<String> {
        if self.errors.is_empty() {
            None
        } else {
            Some(
                self.errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
    }
}

/// Validates all scripts in a list of stubs
pub fn validate_stubs(stubs: &[Stub]) -> StubValidationResult {
    let mut errors = Vec::new();

    for (stub_idx, stub) in stubs.iter().enumerate() {
        let stub_id = stub
            .id
            .clone()
            .unwrap_or_else(|| format!("stub[{stub_idx}]"));

        for (resp_idx, response) in stub.responses.iter().enumerate() {
            if let Some(err) = validate_response(response, &stub_id, resp_idx) {
                errors.push(err);
            }
        }
    }

    StubValidationResult { errors }
}

/// Validates scripts in a single stub
pub fn validate_stub(stub: &Stub, stub_index: usize) -> StubValidationResult {
    let mut errors = Vec::new();

    let stub_id = stub
        .id
        .clone()
        .unwrap_or_else(|| format!("stub[{stub_index}]"));

    for (resp_idx, response) in stub.responses.iter().enumerate() {
        if let Some(err) = validate_response(response, &stub_id, resp_idx) {
            errors.push(err);
        }
    }

    StubValidationResult { errors }
}

/// Validates a single stub response for script errors
fn validate_response(
    response: &StubResponse,
    stub_id: &str,
    response_index: usize,
) -> Option<StubValidationError> {
    match response {
        // Rift script responses (_rift.script)
        StubResponse::RiftScript { rift } => rift.script.as_ref().and_then(|script_config| {
            validate_rift_script_config(script_config, stub_id, response_index)
        }),
        // Is responses with optional _rift extension
        StubResponse::Is { rift, .. } => rift.as_ref().and_then(|rift_ext| {
            rift_ext.script.as_ref().and_then(|script_config| {
                validate_rift_script_config(script_config, stub_id, response_index)
            })
        }),
        // JavaScript inject responses
        StubResponse::Inject { inject } => validate_inject_script(inject, stub_id, response_index),
        // Proxy and Fault responses don't have inline scripts to validate
        StubResponse::Proxy { .. } | StubResponse::Fault { .. } => None,
    }
}

/// Validates a single `_rift.script` config. `code`/`file`/`ref` exactly-one is checked
/// unconditionally; the syntax check only runs against `code` — a `file`/`ref` source is
/// unresolved here (that happens in the config-time resolve-scripts pass, issue #356) so an
/// unresolved script is structurally checked but not syntax-checked by this call site.
fn validate_rift_script_config(
    script_config: &RiftScriptConfig,
    stub_id: &str,
    response_index: usize,
) -> Option<StubValidationError> {
    if !script_config.has_valid_source() {
        return Some(StubValidationError {
            stub_id: stub_id.to_string(),
            response_index,
            engine: script_config.engine.clone().unwrap_or_default(),
            message: format!(
                "script must specify exactly one of `code`, `file`, or `ref` (found {})",
                script_config.source_count()
            ),
        });
    }
    let code = script_config.code.as_deref()?;
    let engine = script_config.engine.as_deref().unwrap_or("rhai");
    validate_rift_script(engine, code, stub_id, response_index)
}

/// Validates a Rift script (_rift.script) using the appropriate validator
fn validate_rift_script(
    engine: &str,
    code: &str,
    stub_id: &str,
    response_index: usize,
) -> Option<StubValidationError> {
    match engine {
        "rhai" => validate_with_validator(
            &super::RhaiValidator::new(),
            code,
            "rhai",
            stub_id,
            response_index,
        ),
        #[cfg(feature = "lua")]
        "lua" => validate_with_validator(
            &super::LuaValidator::new(),
            code,
            "lua",
            stub_id,
            response_index,
        ),
        #[cfg(not(feature = "lua"))]
        "lua" => Some(StubValidationError {
            stub_id: stub_id.to_string(),
            response_index,
            engine: "lua".to_string(),
            message: "Lua engine is not enabled (requires 'lua' feature)".to_string(),
        }),
        #[cfg(feature = "javascript")]
        "javascript" | "js" => validate_with_validator(
            &super::JsValidator::new(),
            code,
            "javascript",
            stub_id,
            response_index,
        ),
        #[cfg(not(feature = "javascript"))]
        "javascript" | "js" => Some(StubValidationError {
            stub_id: stub_id.to_string(),
            response_index,
            engine: "javascript".to_string(),
            message: "JavaScript engine is not enabled (requires 'javascript' feature)".to_string(),
        }),
        other => Some(StubValidationError {
            stub_id: stub_id.to_string(),
            response_index,
            engine: other.to_string(),
            message: format!("Unknown script engine type: '{other}'"),
        }),
    }
}

/// Generic validation using the ScriptValidator trait
fn validate_with_validator<V: ScriptValidator>(
    validator: &V,
    code: &str,
    engine: &str,
    stub_id: &str,
    response_index: usize,
) -> Option<StubValidationError> {
    match validator.validate(code) {
        Ok(()) => None,
        Err(e) => Some(StubValidationError {
            stub_id: stub_id.to_string(),
            response_index,
            engine: engine.to_string(),
            message: e.to_string(),
        }),
    }
}

/// Validates a Mountebank inject script
fn validate_inject_script(
    code: &str,
    stub_id: &str,
    response_index: usize,
) -> Option<StubValidationError> {
    #[cfg(feature = "javascript")]
    {
        // For inject scripts, we validate by wrapping as a variable assignment
        // This matches how the inject is executed at runtime: var __injectFn = {inject_fn};
        use boa_engine::{Context, Source};

        let mut context = Context::default();

        // Wrap the inject function in a variable assignment to validate it
        // This is the same pattern used at runtime
        let wrapper = format!("var __validateFn = {code};");

        match context.eval(Source::from_bytes(wrapper.as_bytes())) {
            Ok(_) => None,
            Err(e) => Some(StubValidationError {
                stub_id: stub_id.to_string(),
                response_index,
                engine: "javascript (inject)".to_string(),
                message: format!("Syntax error: {e}"),
            }),
        }
    }

    #[cfg(not(feature = "javascript"))]
    {
        let _ = (code, stub_id, response_index);
        // If JavaScript feature is not enabled, inject responses won't work at runtime anyway
        // but we can't validate them. We'll let them pass here and fail at runtime.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::{RiftResponseExtension, RiftScriptConfig};

    fn make_rift_script_stub(engine: &str, code: &str) -> Stub {
        Stub {
            id: Some("test-stub".to_string()),
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::RiftScript {
                rift: RiftResponseExtension {
                    fault: None,
                    script: Some(RiftScriptConfig {
                        engine: Some(engine.to_string()),
                        code: Some(code.to_string()),
                        file: None,
                        ref_name: None,
                    }),
                    templated: false,
                },
            }],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    fn make_inject_stub(code: &str) -> Stub {
        Stub {
            id: Some("inject-stub".to_string()),
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::Inject {
                inject: code.to_string(),
            }],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    #[test]
    fn test_valid_rhai_script() {
        let stub = make_rift_script_stub(
            "rhai",
            r#"fn should_inject(request, flow_store) { #{ inject: false } }"#,
        );
        let result = validate_stub(&stub, 0);
        assert!(
            result.is_valid(),
            "Valid Rhai script should pass: {:?}",
            result.errors
        );
    }

    #[test]
    fn test_invalid_rhai_syntax() {
        let stub = make_rift_script_stub(
            "rhai",
            r#"fn should_inject(request, flow_store) { #{ inject: "#, // Missing closing
        );
        let result = validate_stub(&stub, 0);
        assert!(!result.is_valid(), "Invalid syntax should fail");
        assert!(result.errors[0].message.contains("Syntax error"));
    }

    #[test]
    fn test_missing_should_inject_function() {
        let stub = make_rift_script_stub("rhai", r#"fn other_function(x) { x + 1 }"#);
        let result = validate_stub(&stub, 0);
        assert!(!result.is_valid(), "Missing should_inject should fail");
        assert!(result.errors[0].message.contains("should_inject"));
    }

    #[test]
    fn test_unknown_engine() {
        let stub = make_rift_script_stub("unknown_engine", "some code");
        let result = validate_stub(&stub, 0);
        assert!(!result.is_valid(), "Unknown engine should fail");
        assert!(result.errors[0].message.contains("Unknown script engine"));
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_valid_inject_script() {
        let stub = make_inject_stub(r#"function(config, state) { return { statusCode: 200 }; }"#);
        let result = validate_stub(&stub, 0);
        assert!(
            result.is_valid(),
            "Valid inject script should pass: {:?}",
            result.errors
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_invalid_inject_syntax() {
        let stub = make_inject_stub(
            r#"function(config, state) { return { statusCode: "#, // Missing closing
        );
        let result = validate_stub(&stub, 0);
        assert!(!result.is_valid(), "Invalid inject syntax should fail");
    }

    #[test]
    fn test_multiple_stubs_validation() {
        // Create stubs without ids so they get indexed names
        let stubs = vec![
            Stub {
                id: None, // No id, will use stub[0]
                route_pattern: None,
                predicates: vec![],
                responses: vec![StubResponse::RiftScript {
                    rift: RiftResponseExtension {
                        fault: None,
                        script: Some(RiftScriptConfig {
                            engine: Some("rhai".to_string()),
                            code: Some(
                                r#"fn should_inject(request, flow_store) { #{ inject: false } }"#
                                    .to_string(),
                            ),
                            file: None,
                            ref_name: None,
                        }),
                        templated: false,
                    },
                }],
                scenario_name: None,
                required_scenario_state: None,
                new_scenario_state: None,
                space: None,
                recorded_from: None,
                verify: None,
            },
            Stub {
                id: None, // No id, will use stub[1]
                route_pattern: None,
                predicates: vec![],
                responses: vec![StubResponse::RiftScript {
                    rift: RiftResponseExtension {
                        fault: None,
                        script: Some(RiftScriptConfig {
                            engine: Some("rhai".to_string()),
                            code: Some(
                                r#"fn should_inject(request, flow_store) { #{ inject: "#
                                    .to_string(), // Invalid
                            ),
                            file: None,
                            ref_name: None,
                        }),
                        templated: false,
                    },
                }],
                scenario_name: None,
                required_scenario_state: None,
                new_scenario_state: None,
                space: None,
                recorded_from: None,
                verify: None,
            },
        ];
        let result = validate_stubs(&stubs);
        assert!(!result.is_valid());
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].stub_id.contains("stub[1]"),
            "Expected stub[1], got: {}",
            result.errors[0].stub_id
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn test_valid_lua_script() {
        let stub = make_rift_script_stub(
            "lua",
            r#"function should_inject(request, flow_store) return { inject = false } end"#,
        );
        let result = validate_stub(&stub, 0);
        assert!(
            result.is_valid(),
            "Valid Lua script should pass: {:?}",
            result.errors
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn test_invalid_lua_syntax() {
        let stub = make_rift_script_stub(
            "lua",
            r#"function should_inject(request, flow_store) return { inject = "#, // Missing closing
        );
        let result = validate_stub(&stub, 0);
        assert!(!result.is_valid(), "Invalid Lua syntax should fail");
    }
}
