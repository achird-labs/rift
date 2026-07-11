//! Core validation logic for imposter configurations.

use crate::types::{LintIssue, LintOptions, LintResult};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// A JavaScript syntax error surfaced by the embedded validator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct JsSyntaxError(pub String);

/// JavaScript syntax validator using boa_engine.
#[cfg(feature = "javascript")]
mod js_validator {
    use boa_engine::{Context, Script, Source};

    pub fn validate_javascript(script: &str) -> Result<(), super::JsSyntaxError> {
        let mut context = Context::default();

        // Mountebank inject/decorate scripts are anonymous function *expressions*
        // (`function(args){…}` or `function (args){…}`), which are a syntax error when Boa parses
        // them as a top-level statement (a function *declaration* needs a name). Wrap them as an
        // expression so Boa parses them. An anonymous function is `function` followed by optional
        // whitespace and then `(` — the earlier `!contains("function ")` heuristic wrongly treated
        // the common `function (args)` form (a space before the parens) as named, leaving it
        // unwrapped and mis-flagged as a syntax error.
        let script_trimmed = script.trim();
        let is_anonymous_function_expr = script_trimmed
            .strip_prefix("function")
            .map(str::trim_start)
            .is_some_and(|rest| rest.starts_with('('));
        let wrapped = if is_anonymous_function_expr {
            format!("var __fn = ({script_trimmed})")
        } else {
            script_trimmed.to_string()
        };

        // Parse only — never execute. `context.eval` used to *run* top-level statements, so a
        // `while (true) {}` in an inject/decorate body hung the linter with no budget (issue #553).
        // Parsing reports every syntax error and always terminates regardless of the script's
        // runtime behaviour; a parse failure is by definition a real syntax problem, so it is
        // surfaced unconditionally rather than filtered by matching the error message against
        // `"SyntaxError"`/`"unexpected"` (which silently passed anything else).
        match Script::parse(Source::from_bytes(&wrapped), None, &mut context) {
            Ok(_) => Ok(()),
            Err(e) => Err(super::JsSyntaxError(e.to_string())),
        }
    }
}

#[cfg(not(feature = "javascript"))]
mod js_validator {
    #[allow(dead_code)]
    pub fn validate_javascript(_script: &str) -> Result<(), super::JsSyntaxError> {
        Ok(())
    }
}

/// Validate a complete imposter configuration.
pub fn validate_imposter(
    file: &Path,
    imposter: &Value,
    result: &mut LintResult,
    options: &LintOptions,
) {
    check_required_fields(file, imposter, result);
    check_protocol(file, imposter, result);
    check_port_range(file, imposter, result);
    check_state_without_flow_state(file, imposter, result);

    // Named script registry (`_rift.scripts`, issue #356): validated once up front (each entry
    // must be a `code:`/`file:` leaf, not a `ref:`), then handed to every response so a
    // `{ "ref": "name" }` script can be resolved and validated the same way as inline `code:`.
    let registry = imposter
        .get("_rift")
        .and_then(|r| r.get("scripts"))
        .cloned()
        .unwrap_or(Value::Null);
    validate_script_registry(file, &registry, result);

    if let Some(stubs) = imposter.get("stubs").and_then(|v| v.as_array()) {
        for (idx, stub) in stubs.iter().enumerate() {
            validate_stub(file, stub, idx, result, options, &registry);
        }
    }
}

/// Infer a script's effective engine: explicit `engine`, else inferred from a `file` path's
/// extension (`.rhai`/`.js`), else the "rhai" default — mirrors
/// `rift_mock_core::imposter::RiftScriptConfig`'s resolution rule. A `.lua`/`"lua"` engine still
/// infers as `"lua"` here (structural checks only; rift-mock-core rejects it at validate/run time —
/// issue #450).
fn infer_script_engine(explicit: Option<&str>, file_field: Option<&str>) -> String {
    if let Some(e) = explicit {
        return e.to_string();
    }
    let ext = file_field
        .and_then(|f| Path::new(f).extension())
        .and_then(|e| e.to_str());
    match ext {
        Some("rhai") => "rhai",
        Some("lua") => "lua",
        Some("js") => "javascript",
        _ => "rhai",
    }
    .to_string()
}

/// Read a `file:` script path relative to the linted config file's own directory.
fn read_script_file_relative(config_file: &Path, rel: &str) -> std::io::Result<String> {
    let dir = config_file.parent().unwrap_or_else(|| Path::new("."));
    std::fs::read_to_string(dir.join(rel))
}

/// Syntax-check a resolved script's content at the same level inline `code:` would get for its
/// engine. Only "javascript" has an embedded syntax checker in this crate (`js_validator`,
/// gated behind the `javascript` feature) — rhai scripts (and any other engine string) get the
/// structural checks above (exactly-one-source, ref resolution, file existence) but no deep
/// syntax check here.
fn check_script_syntax(
    file: &Path,
    code: &str,
    engine: &str,
    location: &str,
    result: &mut LintResult,
) {
    if (engine == "javascript" || engine == "js")
        && let Err(e) = js_validator::validate_javascript(code)
    {
        result.add_issue(
            LintIssue::error(
                "E040",
                format!("JavaScript syntax error in _rift.script: {e}"),
                file.to_path_buf(),
            )
            .with_location(location),
        );
    }
}

/// The `_rift.fault.tcp` fault kinds Rift accepts, canonical names plus short aliases — mirrors
/// `TcpFaultKind::parse` (rift-mock-core `imposter/fault_io.rs`). Kept in sync by hand because
/// rift-lint does not depend on rift-mock-core.
const TCP_FAULT_KINDS: &[&str] = &[
    "reset",
    "CONNECTION_RESET_BY_PEER",
    "empty",
    "EMPTY_RESPONSE",
    "garbage",
    "random",
    "RANDOM_DATA_THEN_CLOSE",
    "malformed",
    "MALFORMED_RESPONSE_CHUNK",
];

/// Validate a `_rift.fault.tcp` value (issue #531). Accepts the bare kind string and the
/// probabilistic object form `{ probability, type }`. The object form must carry a numeric
/// `probability` in `[0, 1]` (E041) — it exists solely to express that probability. An unknown
/// fault type is a warning (W011), mirroring the serve-time `warn!` rather than failing the config.
fn validate_tcp_fault(file: &Path, tcp: &Value, location: &str, result: &mut LintResult) {
    let warn_unknown_kind = |result: &mut LintResult, kind: &str| {
        if !TCP_FAULT_KINDS.contains(&kind) {
            result.add_issue(
                LintIssue::warning(
                    "W011",
                    format!("Unknown TCP fault type '{kind}' — the fault will not fire at runtime"),
                    file.to_path_buf(),
                )
                .with_location(location.to_string())
                .with_suggestion(format!("Use one of: {}", TCP_FAULT_KINDS.join(", "))),
            );
        }
    };

    match tcp {
        Value::String(kind) => warn_unknown_kind(result, kind),
        Value::Object(_) => {
            match tcp.get("probability").and_then(Value::as_f64) {
                Some(p) if (0.0..=1.0).contains(&p) => {}
                Some(_) => result.add_issue(
                    LintIssue::error(
                        "E041",
                        "_rift.fault.tcp 'probability' must be between 0.0 and 1.0",
                        file.to_path_buf(),
                    )
                    .with_location(location.to_string()),
                ),
                None => result.add_issue(
                    LintIssue::error(
                        "E041",
                        "_rift.fault.tcp object form requires a numeric 'probability' (use the bare string form for an always-firing fault)",
                        file.to_path_buf(),
                    )
                    .with_location(location.to_string())
                    .with_suggestion(
                        "Add \"probability\": <0.0-1.0>, or replace the object with the bare fault-type string",
                    ),
                ),
            }
            match tcp.get("type").and_then(Value::as_str) {
                Some(kind) => warn_unknown_kind(result, kind),
                None => result.add_issue(
                    LintIssue::error(
                        "E041",
                        "_rift.fault.tcp object form requires a string 'type'",
                        file.to_path_buf(),
                    )
                    .with_location(location.to_string()),
                ),
            }
        }
        _ => result.add_issue(
            LintIssue::error(
                "E041",
                "_rift.fault.tcp must be a fault-type string or an object { probability, type }",
                file.to_path_buf(),
            )
            .with_location(location.to_string()),
        ),
    }
}

/// Validate one `_rift.script`-shaped object: `{ engine?, code?, file?, ref? }` (issue #356).
/// Exactly one of `code`/`file`/`ref` must be present (E036). A `file:` is read relative to
/// `config_file`'s own directory (E038 if unreadable); a `ref:` is resolved against `registry`
/// (`Value::Null` when validating a registry entry itself, which cannot use `ref`) — E037 if the
/// name is unknown. Whatever content is resolved gets the same syntax check inline `code:` would.
fn validate_script_source(
    config_file: &Path,
    script: &Value,
    location: &str,
    result: &mut LintResult,
    registry: &Value,
) {
    let code_field = script.get("code").and_then(|v| v.as_str());
    let file_field = script.get("file").and_then(|v| v.as_str());
    let ref_field = script.get("ref").and_then(|v| v.as_str());
    let engine_field = script.get("engine").and_then(|v| v.as_str());

    let source_count = [
        code_field.is_some(),
        file_field.is_some(),
        ref_field.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    if source_count != 1 {
        result.add_issue(
            LintIssue::error(
                "E036",
                format!(
                    "script must specify exactly one of 'code', 'file', or 'ref' (found {source_count})"
                ),
                config_file.to_path_buf(),
            )
            .with_location(location)
            .with_suggestion("Set exactly one of 'code', 'file', or 'ref'"),
        );
        return;
    }

    if let Some(ref_name) = ref_field {
        let Some(target) = registry.get(ref_name) else {
            result.add_issue(
                LintIssue::error(
                    "E037",
                    format!(
                        "Unknown script ref '{ref_name}': no entry named '{ref_name}' in _rift.scripts"
                    ),
                    config_file.to_path_buf(),
                )
                .with_location(location)
                .with_suggestion("Add this name under _rift.scripts, or fix the typo"),
            );
            return;
        };
        // The target's own exactly-one-source / no-ref-chain checks already ran in
        // `validate_script_registry`; here we only need its resolved code to syntax-check what
        // this `ref:` actually points at.
        let target_code = target.get("code").and_then(|v| v.as_str());
        let target_file = target.get("file").and_then(|v| v.as_str());
        let target_engine = target.get("engine").and_then(|v| v.as_str());
        let resolved = match (target_code, target_file) {
            (Some(c), _) => Some(c.to_string()),
            (None, Some(f)) => match read_script_file_relative(config_file, f) {
                Ok(content) => Some(content),
                Err(e) => {
                    result.add_issue(
                        LintIssue::error(
                            "E038",
                            format!(
                                "script file '{f}' (via ref '{ref_name}') could not be read: {e}"
                            ),
                            config_file.to_path_buf(),
                        )
                        .with_location(location),
                    );
                    return;
                }
            },
            _ => None,
        };
        if let Some(code) = resolved {
            let engine = infer_script_engine(target_engine, target_file);
            check_script_syntax(config_file, &code, &engine, location, result);
        }
        return;
    }

    if let Some(f) = file_field {
        match read_script_file_relative(config_file, f) {
            Ok(content) => {
                let engine = infer_script_engine(engine_field, Some(f));
                check_script_syntax(config_file, &content, &engine, location, result);
            }
            Err(e) => {
                result.add_issue(
                    LintIssue::error(
                        "E038",
                        format!("script file '{f}' could not be read: {e}"),
                        config_file.to_path_buf(),
                    )
                    .with_location(location)
                    .with_suggestion("Check the path is relative to the config file"),
                );
            }
        }
        return;
    }

    if let Some(code) = code_field {
        let engine = infer_script_engine(engine_field, None);
        check_script_syntax(config_file, code, &engine, location, result);
    }
}

/// Validate every entry in the `_rift.scripts` registry: each must be a `code:`/`file:` leaf
/// script (never `ref:` — no chains), and its source resolved/checked exactly like an inline
/// stub response script.
fn validate_script_registry(file: &Path, registry: &Value, result: &mut LintResult) {
    let Some(entries) = registry.as_object() else {
        return;
    };
    for (name, entry) in entries {
        let location = format!("_rift.scripts.{name}");
        if entry.get("ref").and_then(|v| v.as_str()).is_some() {
            result.add_issue(
                LintIssue::error(
                    "E039",
                    format!(
                        "_rift.scripts entry '{name}' may not itself use 'ref' (ref chains are not allowed)"
                    ),
                    file.to_path_buf(),
                )
                .with_location(location),
            );
            continue;
        }
        validate_script_source(file, entry, &location, result, &Value::Null);
    }
}

/// Check that required fields are present.
fn check_required_fields(file: &Path, imposter: &Value, result: &mut LintResult) {
    let required = ["port", "protocol", "stubs"];

    for field in required {
        if imposter.get(field).is_none() {
            result.add_issue(
                LintIssue::error(
                    "E003",
                    format!("Missing required field: {field}"),
                    file.to_path_buf(),
                )
                .with_suggestion(format!("Add \"{field}\" to the imposter configuration")),
            );
        }
    }
}

/// Check that the protocol is valid.
fn check_protocol(file: &Path, imposter: &Value, result: &mut LintResult) {
    if let Some(protocol) = imposter.get("protocol").and_then(|v| v.as_str()) {
        if !["http", "https", "tcp"].contains(&protocol) {
            result.add_issue(
                LintIssue::error(
                    "E004",
                    format!("Invalid protocol: {protocol}"),
                    file.to_path_buf(),
                )
                .with_location("protocol")
                .with_suggestion("Use 'http' or 'https' (tcp is not yet supported by Rift)"),
            );
        } else if protocol == "tcp" {
            result.add_issue(
                LintIssue::warning(
                    "W010",
                    "Protocol 'tcp' is not yet implemented by Rift and will fail at runtime",
                    file.to_path_buf(),
                )
                .with_location("protocol")
                .with_suggestion("Use 'http' or 'https' instead"),
            );
        }
    }
}

/// Check that the port is in a valid range.
fn check_port_range(file: &Path, imposter: &Value, result: &mut LintResult) {
    if let Some(port) = imposter.get("port").and_then(|v| v.as_u64()) {
        if !(1..=65535).contains(&port) {
            result.add_issue(
                LintIssue::error(
                    "E005",
                    format!("Port {port} is out of valid range (1-65535)"),
                    file.to_path_buf(),
                )
                .with_location("port"),
            );
        } else if port < 1024 {
            result.add_issue(
                LintIssue::warning(
                    "W001",
                    format!("Port {port} is a privileged port (requires root)"),
                    file.to_path_buf(),
                )
                .with_location("port")
                .with_suggestion("Consider using a port >= 1024"),
            );
        }
    }
}

/// Best-effort resolve of a `{ engine?, code?, file?, ref? }` script object's source text, for the
/// E042 heuristic below. Unlike [`validate_script_source`] this doesn't itself report issues on a
/// resolution failure (unreadable file, unknown ref) — those are already reported elsewhere by the
/// real validation pass; here an unresolvable script is simply skipped.
fn resolve_script_text(config_file: &Path, script: &Value, registry: &Value) -> Option<String> {
    if let Some(code) = script.get("code").and_then(|v| v.as_str()) {
        return Some(code.to_string());
    }
    if let Some(f) = script.get("file").and_then(|v| v.as_str()) {
        return read_script_file_relative(config_file, f).ok();
    }
    if let Some(r) = script.get("ref").and_then(|v| v.as_str()) {
        let target = registry.get(r)?;
        return resolve_script_text(config_file, target, &Value::Null);
    }
    None
}

/// Issue #358: a script that calls `ctx.state` (or the v1 `flow_store`) needs a flow store to
/// persist its writes. Without `_rift.flowState` configured, `Imposter::create_flow_store`
/// auto-provisions an in-memory store instead of a silent no-op — state works, but only for this
/// process's lifetime and isn't shared across a cluster. Warn so that's a deliberate choice, not a
/// surprise at scale.
fn check_state_without_flow_state(file: &Path, imposter: &Value, result: &mut LintResult) {
    let has_flow_state = imposter
        .get("_rift")
        .and_then(|r| r.get("flowState"))
        .is_some();
    if has_flow_state {
        return;
    }

    let registry = imposter
        .get("_rift")
        .and_then(|r| r.get("scripts"))
        .cloned()
        .unwrap_or(Value::Null);

    let Some(stubs) = imposter.get("stubs").and_then(|v| v.as_array()) else {
        return;
    };
    for (idx, stub) in stubs.iter().enumerate() {
        let Some(responses) = stub.get("responses").and_then(|v| v.as_array()) else {
            continue;
        };
        for (resp_idx, response) in responses.iter().enumerate() {
            let Some(script) = response.get("_rift").and_then(|rift| rift.get("script")) else {
                continue;
            };
            let Some(code) = resolve_script_text(file, script, &registry) else {
                continue;
            };
            if code.contains("ctx.state") || code.contains("flow_store") {
                result.add_issue(
                    LintIssue::warning(
                        "E042",
                        "Script uses ctx.state (or flow_store) but no _rift.flowState is configured",
                        file.to_path_buf(),
                    )
                    .with_location(format!("stubs[{idx}].responses[{resp_idx}]._rift.script"))
                    .with_suggestion(
                        "State will be auto-provisioned in-memory (won't persist across restarts \
                         or be shared across a cluster) — configure _rift.flowState for production",
                    ),
                );
            }
        }
    }
}

/// Validate a single stub.
pub fn validate_stub(
    file: &Path,
    stub: &Value,
    idx: usize,
    result: &mut LintResult,
    options: &LintOptions,
    registry: &Value,
) {
    let location = format!("stubs[{idx}]");

    if let Some(predicates) = stub.get("predicates").and_then(|v| v.as_array()) {
        for (pred_idx, predicate) in predicates.iter().enumerate() {
            validate_predicate(
                file,
                predicate,
                &format!("{location}.predicates[{pred_idx}]"),
                result,
                options,
            );
        }
    }

    if let Some(responses) = stub.get("responses").and_then(|v| v.as_array()) {
        if responses.is_empty() {
            result.add_issue(
                LintIssue::warning("W002", "Stub has no responses defined", file.to_path_buf())
                    .with_location(&location)
                    .with_suggestion("Add at least one response"),
            );
        }

        for (resp_idx, response) in responses.iter().enumerate() {
            validate_response(
                file,
                response,
                &format!("{location}.responses[{resp_idx}]"),
                result,
                options,
                registry,
            );
        }
    } else {
        result.add_issue(
            LintIssue::error("E006", "Stub missing 'responses' field", file.to_path_buf())
                .with_location(location),
        );
    }
}

/// Validate a predicate.
pub fn validate_predicate(
    file: &Path,
    predicate: &Value,
    location: &str,
    result: &mut LintResult,
    options: &LintOptions,
) {
    let valid_operators = [
        "equals",
        "deepEquals",
        "contains",
        "startsWith",
        "endsWith",
        "matches",
        "exists",
        "not",
        "or",
        "and",
        "inject",
    ];

    let Some(pred_obj) = predicate.as_object() else {
        result.add_issue(
            LintIssue::error("E007", "Predicate must be an object", file.to_path_buf())
                .with_location(location),
        );
        return;
    };

    let modifier_keys: HashSet<&str> =
        HashSet::from(["jsonpath", "xpath", "caseSensitive", "except"]);
    let operator_names: Vec<&str> = pred_obj
        .keys()
        .map(|k| k.as_str())
        .filter(|&k| !modifier_keys.contains(k))
        .filter(|&k| {
            let is_valid_op = valid_operators.contains(&k);
            if !is_valid_op {
                result.add_issue(
                    LintIssue::error(
                        "E009",
                        format!("Unknown predicate operator: {k}"),
                        file.to_path_buf(),
                    )
                    .with_location(location)
                    .with_suggestion(format!("Use one of: {}", valid_operators.join(", "))),
                );
            }
            is_valid_op
        })
        .collect();
    if operator_names.is_empty() {
        result.add_issue(
            LintIssue::error("E008", "Predicate has no operator", file.to_path_buf())
                .with_location(location)
                .with_suggestion(format!("Add one of: {}", valid_operators.join(", "))),
        );
        return;
    }
    if operator_names.len() > 1 {
        let sub_predicates = operator_names
            .iter()
            .map(|&k| serde_json::json!({k: "..."}))
            .collect::<Vec<_>>();
        let sub_predicates = serde_json::json!(sub_predicates);
        let and_predicate = serde_json::json!({"and": sub_predicates});
        let suggestion = format!(
            "Split into separate predicates, combined with and, like: {and_predicate} (or just {sub_predicates} if at the top level)"
        );
        result.add_issue(
            LintIssue::error("E034", "Only one predicate operation", file.to_path_buf())
                .with_location(location)
                .with_suggestion(suggestion),
        );
    }

    if let Some(jsonpath) = predicate.get("jsonpath") {
        validate_jsonpath(file, jsonpath, location, result);
    }

    if let Some(matches) = predicate.get("matches") {
        validate_regex_patterns(file, matches, location, result, options);
    }

    // Recursively validate nested predicates
    for key in ["and", "or", "not"] {
        if let Some(nested) = predicate.get(key) {
            if key == "not" {
                // `not` wraps a single predicate object — pass it directly
                validate_predicate(file, nested, &format!("{location}.not"), result, options);
            } else if let Some(nested_array) = nested.as_array() {
                for (i, nested_pred) in nested_array.iter().enumerate() {
                    validate_predicate(
                        file,
                        nested_pred,
                        &format!("{location}.{key}[{i}]"),
                        result,
                        options,
                    );
                }
            }
        }
    }
}

/// Validate JSONPath selector.
fn validate_jsonpath(file: &Path, jsonpath: &Value, location: &str, result: &mut LintResult) {
    if let Some(selector) = jsonpath.get("selector").and_then(|v| v.as_str()) {
        let slice_re = Regex::new(r"\[:(\d+)\]").unwrap();
        if slice_re.is_match(selector) {
            result.add_issue(
                LintIssue::info(
                    "I001",
                    format!("JSONPath uses Mountebank slice notation: {selector}"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.jsonpath.selector"))
                .with_suggestion("This is supported by Rift but not standard JSONPath"),
            );
        }

        let open_brackets = selector.chars().filter(|c| *c == '[').count();
        let close_brackets = selector.chars().filter(|c| *c == ']').count();
        if open_brackets != close_brackets {
            result.add_issue(
                LintIssue::error(
                    "E010",
                    "Unbalanced brackets in JSONPath selector",
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.jsonpath.selector")),
            );
        }
    } else {
        result.add_issue(
            LintIssue::error(
                "E011",
                "JSONPath missing 'selector' field",
                file.to_path_buf(),
            )
            .with_location(format!("{location}.jsonpath")),
        );
    }
}

/// Validate regex patterns in matches predicate.
fn validate_regex_patterns(
    file: &Path,
    matches: &Value,
    location: &str,
    result: &mut LintResult,
    _options: &LintOptions,
) {
    if let Some(obj) = matches.as_object() {
        for (field, pattern) in obj {
            if let Some(pattern_str) = pattern.as_str()
                && let Err(e) = Regex::new(pattern_str)
            {
                result.add_issue(
                    LintIssue::error(
                        "E013",
                        format!("Invalid regex pattern in '{field}': {e}"),
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.matches.{field}"))
                    .with_suggestion("Check regex syntax"),
                );
            }
        }
    }
}

/// Validate a response object.
pub fn validate_response(
    file: &Path,
    response: &Value,
    location: &str,
    result: &mut LintResult,
    options: &LintOptions,
    registry: &Value,
) {
    let has_is = response.get("is").is_some();
    let has_proxy = response
        .get("proxy")
        .map(|p| !p.is_null() && p.is_object() && p.get("to").is_some())
        .unwrap_or(false);
    let has_inject = response.get("inject").is_some();
    let has_fault = response.get("fault").is_some();
    let has_rift = response.get("_rift").is_some();

    if has_rift {
        result.add_issue(
            LintIssue::info(
                "I003",
                "Response uses Rift '_rift' extension (not Mountebank-compatible)",
                file.to_path_buf(),
            )
            .with_location(location),
        );
    }

    // `_rift.script` `file:`/`ref:` sources are validated exactly like inline `code:` (issue
    // #356): read/resolved, then syntax-checked at whatever level the engine supports.
    if let Some(script) = response.get("_rift").and_then(|rift| rift.get("script")) {
        validate_script_source(
            file,
            script,
            &format!("{location}._rift.script"),
            result,
            registry,
        );
    }

    // `_rift.fault.tcp` accepts both the bare kind string and the probabilistic object form
    // (issue #531). Validate the object form's `probability` and warn on an unknown fault type.
    if let Some(tcp) = response
        .get("_rift")
        .and_then(|rift| rift.get("fault"))
        .and_then(|fault| fault.get("tcp"))
    {
        validate_tcp_fault(file, tcp, &format!("{location}._rift.fault.tcp"), result);
    }

    let response_types = [has_is, has_proxy, has_inject, has_fault, has_rift];
    let active_types = response_types.iter().filter(|&&t| t).count();

    if active_types == 0 {
        result.add_issue(
            LintIssue::error(
                "E014",
                "Response has no response type (is, proxy, inject, fault, or _rift)",
                file.to_path_buf(),
            )
            .with_location(location)
            .with_suggestion(
                "Add 'is', 'proxy', 'inject', 'fault', or '_rift' to define the response",
            ),
        );
    } else if active_types > 1 && has_is && has_proxy {
        let proxy_val = response.get("proxy");
        if proxy_val.map(|p| !p.is_null()).unwrap_or(false) {
            result.add_issue(
                LintIssue::warning(
                    "W003",
                    "Response has both 'is' and 'proxy' defined",
                    file.to_path_buf(),
                )
                .with_location(location)
                .with_suggestion("Use either 'is' for static responses or 'proxy' for forwarding"),
            );
        }
    }

    if let Some(is_response) = response.get("is") {
        validate_is_response(file, is_response, &format!("{location}.is"), result);
    }

    if let Some(proxy) = response.get("proxy")
        && !proxy.is_null()
    {
        validate_proxy_response(file, proxy, &format!("{location}.proxy"), result);
    }

    // Rift/Mountebank write behaviors as `_behaviors: { wait, repeat, ... }` (object).
    // Rift also accepts and serializes `behaviors: [...]` (array) for MB compatibility.
    // Validate whichever form is present.
    if let Some(b) = response.get("_behaviors") {
        if b.is_object() {
            validate_behavior(file, b, &format!("{location}._behaviors"), result, options);
        }
    } else if let Some(behaviors) = response.get("behaviors").and_then(|v| v.as_array()) {
        for (idx, behavior) in behaviors.iter().enumerate() {
            validate_behavior(
                file,
                behavior,
                &format!("{location}.behaviors[{idx}]"),
                result,
                options,
            );
        }
    }
}

/// Validate an "is" response.
pub fn validate_is_response(
    file: &Path,
    is_response: &Value,
    location: &str,
    result: &mut LintResult,
) {
    if let Some(status) = is_response.get("statusCode") {
        let status_num = status
            .as_u64()
            .or_else(|| status.as_str().and_then(|s| s.parse().ok()));

        match status_num {
            Some(code) if !(100..=599).contains(&code) => {
                result.add_issue(
                    LintIssue::error(
                        "E015",
                        format!("Invalid HTTP status code: {code}"),
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.statusCode"))
                    .with_suggestion("Use a valid HTTP status code (100-599)"),
                );
            }
            None => {
                result.add_issue(
                    LintIssue::error(
                        "E016",
                        "statusCode must be a number or numeric string",
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.statusCode")),
                );
            }
            _ => {}
        }
    }

    if let Some(headers) = is_response.get("headers") {
        validate_headers(file, headers, &format!("{location}.headers"), result);
    }

    // Check if body is valid JSON when Content-Type is application/json
    if let Some(body) = is_response.get("body")
        && let Some(headers) = is_response.get("headers").and_then(|h| h.as_object())
    {
        let content_type = headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == "content-type")
            .and_then(|(_, v)| v.as_str());

        if content_type
            .map(|ct| ct.contains("application/json"))
            .unwrap_or(false)
            && let Some(body_str) = body.as_str()
            && serde_json::from_str::<Value>(body_str).is_err()
        {
            result.add_issue(
                LintIssue::warning(
                    "W004",
                    "Body is not valid JSON but Content-Type is application/json",
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.body"))
                .with_suggestion("Verify the body is valid JSON"),
            );
        }
    }
}

/// Validate response headers.
pub fn validate_headers(file: &Path, headers: &Value, location: &str, result: &mut LintResult) {
    let Some(headers_obj) = headers.as_object() else {
        result.add_issue(
            LintIssue::error("E021", "Headers must be an object", file.to_path_buf())
                .with_location(location),
        );
        return;
    };

    for (name, value) in headers_obj {
        if name.is_empty() {
            result.add_issue(
                LintIssue::error("E017", "Empty header name", file.to_path_buf())
                    .with_location(location),
            );
        }

        if let Some(elements) = value.as_array() {
            // Arrays of strings are valid multi-value headers (e.g. Set-Cookie),
            // supported by the engine since #238. Only non-string elements are invalid.
            if !elements.iter().all(Value::is_string) {
                result.add_issue(
                    LintIssue::error(
                        "E018",
                        format!(
                            "Header '{name}' array contains a non-string element, \
                             all multi-value header entries must be strings"
                        ),
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.{name}"))
                    .with_suggestion("Use an array of strings, e.g. [\"a=1\", \"b=2\"]"),
                );
            }
        } else if value.is_number() {
            result.add_issue(
                LintIssue::error(
                    "E019",
                    format!("Header '{name}' value is a number, must be a string"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.{name}"))
                .with_suggestion(format!("Change to: \"{name}\": \"{value}\"")),
            );
        } else if value.is_boolean() {
            result.add_issue(
                LintIssue::error(
                    "E020",
                    format!("Header '{name}' value is a boolean, must be a string"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.{name}"))
                .with_suggestion(format!("Change to: \"{name}\": \"{value}\"")),
            );
        } else if value.is_null() {
            result.add_issue(
                LintIssue::warning(
                    "W005",
                    format!("Header '{name}' value is null"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.{name}"))
                .with_suggestion("Remove header or set a string value"),
            );
        }

        if name.to_lowercase() == "content-length"
            && let Some(len_str) = value.as_str()
            && let Ok(len) = len_str.parse::<u64>()
            && len < 10
        {
            result.add_issue(
                LintIssue::warning(
                    "W006",
                    format!("Content-Length is very small ({len}), may cause issues"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.{name}"))
                .with_suggestion("Verify Content-Length matches actual body length"),
            );
        }
    }
}

/// Validate a proxy response.
pub fn validate_proxy_response(
    file: &Path,
    proxy: &Value,
    location: &str,
    result: &mut LintResult,
) {
    if let Some(to) = proxy.get("to") {
        if let Some(url) = to.as_str() {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                result.add_issue(
                    LintIssue::error(
                        "E022",
                        format!("Proxy 'to' URL must start with http:// or https://: {url}"),
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.to")),
                );
            }

            if url.contains("localhost:") || url.contains("127.0.0.1:") {
                let port_re = Regex::new(r":(\d+)").unwrap();
                if let Some(captures) = port_re.captures(url)
                    && let Ok(port) = captures[1].parse::<u16>()
                    && port > 10000
                {
                    result.add_issue(
                        LintIssue::info(
                            "I002",
                            format!("Proxy targets localhost:{port}"),
                            file.to_path_buf(),
                        )
                        .with_location(format!("{location}.to"))
                        .with_suggestion("Ensure upstream service is running on this port"),
                    );
                }
            }
        } else {
            result.add_issue(
                LintIssue::error(
                    "E023",
                    "Proxy 'to' must be a string URL",
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.to")),
            );
        }
    } else {
        result.add_issue(
            LintIssue::error(
                "E024",
                "Proxy missing required 'to' field",
                file.to_path_buf(),
            )
            .with_location(location),
        );
    }

    if let Some(mode) = proxy.get("mode").and_then(|v| v.as_str()) {
        let valid_modes = ["proxyOnce", "proxyAlways", "proxyTransparent"];
        if !valid_modes.contains(&mode) {
            result.add_issue(
                LintIssue::warning(
                    "W007",
                    format!("Unknown proxy mode: {mode}"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.mode"))
                .with_suggestion(format!("Use one of: {}", valid_modes.join(", "))),
            );
        }
    }
}

/// Validate a behavior.
pub fn validate_behavior(
    file: &Path,
    behavior: &Value,
    location: &str,
    result: &mut LintResult,
    options: &LintOptions,
) {
    let Some(obj) = behavior.as_object() else {
        return;
    };

    if let Some(wait) = obj.get("wait") {
        if let Some(script) = wait.as_str() {
            validate_javascript_behavior(
                file,
                script,
                &format!("{location}.wait"),
                result,
                options,
                false,
            );
        } else if wait.is_number() {
            // fixed millisecond delay — valid
        } else if is_valid_wait_range(wait) {
            // {min, max} range object — valid Rift extension
        } else {
            result.add_issue(
                LintIssue::error(
                    "E025",
                    "Wait behavior must be a number, JavaScript function string, or {min, max} object",
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.wait"))
                .with_suggestion("Use a millisecond number, a JS function string, or {\"min\": N, \"max\": M}"),
            );
        }
    }

    if let Some(repeat) = obj.get("repeat") {
        let valid = repeat.as_u64().map(|n| n > 0).unwrap_or(false);
        if !valid {
            result.add_issue(
                LintIssue::error(
                    "E035",
                    "Repeat behavior must be a positive integer",
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.repeat"))
                .with_suggestion("Use a positive integer, e.g. \"repeat\": 3"),
            );
        }
    }

    if let Some(decorate) = obj.get("decorate")
        && let Some(script) = decorate.as_str()
    {
        validate_javascript_behavior(
            file,
            script,
            &format!("{location}.decorate"),
            result,
            options,
            true,
        );
    }

    if let Some(shell) = obj.get("shellTransform")
        && let Some(cmd) = shell.as_str()
    {
        let dangerous_patterns = ["rm ", "rm -", "sudo ", "chmod ", "dd ", "> /dev/"];
        for pattern in dangerous_patterns {
            if cmd.contains(pattern) {
                result.add_issue(
                    LintIssue::warning(
                        "W008",
                        format!("shellTransform contains potentially dangerous command: {pattern}"),
                        file.to_path_buf(),
                    )
                    .with_location(format!("{location}.shellTransform"))
                    .with_suggestion("Review this command for safety"),
                );
            }
        }
    }

    if let Some(copy) = obj.get("copy") {
        validate_copy_behavior(file, copy, &format!("{location}.copy"), result);
    }

    if let Some(lookup) = obj.get("lookup") {
        validate_lookup_behavior(file, lookup, &format!("{location}.lookup"), result);
    }
}

/// Validate JavaScript in a behavior. `allow_rhai` is set for behaviors that also accept a Rhai
/// script (`decorate`): the engine routes any non-function decorate body — including the Mountebank
/// `config =>` convention and bare Rhai — to its script engine (`apply_js_or_rhai_decorate`), so
/// the "should be a function expression" nudge (W009) must not fire there (issues #248/#257). For
/// JS-only behaviors (`wait`) it stays off, and a non-function script still warns.
fn validate_javascript_behavior(
    file: &Path,
    script: &str,
    location: &str,
    result: &mut LintResult,
    _options: &LintOptions,
    allow_rhai: bool,
) {
    let script_trimmed = script.trim();

    if !allow_rhai && !script_trimmed.starts_with("function") && !script_trimmed.is_empty() {
        result.add_issue(
            LintIssue::warning(
                "W009",
                "JavaScript behavior should be a function expression",
                file.to_path_buf(),
            )
            .with_location(location)
            .with_suggestion("Wrap code in: function() { ... }"),
        );
    }

    let open_braces = script.chars().filter(|c| *c == '{').count();
    let close_braces = script.chars().filter(|c| *c == '}').count();
    if open_braces != close_braces {
        result.add_issue(
            LintIssue::error(
                "E026",
                "Unbalanced braces in JavaScript",
                file.to_path_buf(),
            )
            .with_location(location),
        );
    }

    let open_parens = script.chars().filter(|c| *c == '(').count();
    let close_parens = script.chars().filter(|c| *c == ')').count();
    if open_parens != close_parens {
        result.add_issue(
            LintIssue::error(
                "E027",
                "Unbalanced parentheses in JavaScript",
                file.to_path_buf(),
            )
            .with_location(location),
        );
    }

    #[cfg(feature = "javascript")]
    {
        if let Err(e) = js_validator::validate_javascript(script) {
            result.add_issue(
                LintIssue::error(
                    "E028",
                    format!("JavaScript syntax error: {e}"),
                    file.to_path_buf(),
                )
                .with_location(location),
            );
        }
    }
}

/// Check whether a wait value is a valid {min, max} range object.
fn is_valid_wait_range(wait: &Value) -> bool {
    let Some(obj) = wait.as_object() else {
        return false;
    };
    obj.get("min").and_then(|v| v.as_u64()).is_some()
        && obj.get("max").and_then(|v| v.as_u64()).is_some()
}

/// Validate copy behavior.
fn validate_copy_behavior(file: &Path, copy: &Value, location: &str, result: &mut LintResult) {
    if let Some(arr) = copy.as_array() {
        for (idx, item) in arr.iter().enumerate() {
            if let Some(obj) = item.as_object() {
                if obj.get("from").is_none() {
                    result.add_issue(
                        LintIssue::error(
                            "E029",
                            "Copy behavior item missing 'from' field",
                            file.to_path_buf(),
                        )
                        .with_location(format!("{location}[{idx}]")),
                    );
                }
                if obj.get("into").is_none() {
                    result.add_issue(
                        LintIssue::error(
                            "E030",
                            "Copy behavior item missing 'into' field",
                            file.to_path_buf(),
                        )
                        .with_location(format!("{location}[{idx}]")),
                    );
                }
            }
        }
    }
}

/// Validate lookup behavior.
fn validate_lookup_behavior(file: &Path, lookup: &Value, location: &str, result: &mut LintResult) {
    if let Some(obj) = lookup.as_object() {
        if obj.get("key").is_none() {
            result.add_issue(
                LintIssue::error(
                    "E031",
                    "Lookup behavior missing 'key' field",
                    file.to_path_buf(),
                )
                .with_location(location),
            );
        }
        if obj.get("fromDataSource").is_none() {
            result.add_issue(
                LintIssue::error(
                    "E032",
                    "Lookup behavior missing 'fromDataSource' field",
                    file.to_path_buf(),
                )
                .with_location(location),
            );
        }
        if obj.get("into").is_none() {
            result.add_issue(
                LintIssue::error(
                    "E033",
                    "Lookup behavior missing 'into' field",
                    file.to_path_buf(),
                )
                .with_location(location),
            );
        }
    }
}

#[cfg(test)]
mod js_behavior_tests {
    use super::validate_javascript_behavior;
    use crate::types::{LintOptions, LintResult};
    use std::path::Path;

    fn lint(script: &str) -> LintResult {
        let mut result = LintResult::new();
        validate_javascript_behavior(
            Path::new("test.json"),
            script,
            "loc",
            &mut result,
            &LintOptions::default(),
            false,
        );
        result
    }

    fn has_code(result: &LintResult, code: &str) -> bool {
        result.issues.iter().any(|i| i.code == code)
    }

    #[test]
    fn w009_not_fired_for_arrow_config_decorate() {
        // Mountebank `config =>` decorate convention, valid since #191/#248.
        for script in [
            "config => { config.response.statusCode = 202; }",
            "config=>{ config.response.statusCode = 202; }",
            "(config) => { config.response.body = 'x'; }",
            "(config)=>{ config.response.body = 'x'; }",
        ] {
            assert!(
                !has_code(&lint_decorate(script), "W009"),
                "W009 fired for: {script}"
            );
        }
    }

    #[test]
    fn w009_not_fired_for_function_config_decorate() {
        for script in [
            "function(config) { config.response.statusCode = 202; }",
            "function (config) { config.response.statusCode = 202; }",
        ] {
            assert!(
                !has_code(&lint_decorate(script), "W009"),
                "W009 fired for: {script}"
            );
        }
    }

    #[test]
    fn e028_not_fired_for_anonymous_function_expressions() {
        // Mountebank inject/decorate scripts are anonymous function expressions. Both the
        // no-space and space-before-parens forms are valid and the engine runs them; the JS
        // syntax validator must wrap them as expressions rather than mis-flag `function (args)`
        // as a nameless declaration (E028). Meaningful under the `javascript` feature; a no-op
        // otherwise.
        for script in [
            "function(config, state) { state.n = (state.n||0)+1; return { statusCode: 200 }; }",
            "function (config, state) { state.n = (state.n||0)+1; return { statusCode: 200 }; }",
            "function (request, response) { response.headers['X-D'] = 'r'; }",
            "function(){ return 0; }",
        ] {
            assert!(
                !has_code(&lint(script), "E028"),
                "E028 (JS syntax error) wrongly fired for a valid anonymous function: {script}"
            );
        }
    }

    #[test]
    fn w009_not_fired_for_bare_config_body() {
        // A decorate body that just mutates `config.response`/`config.request`.
        assert!(!has_code(
            &lint_decorate("config.response.statusCode = 404;"),
            "W009"
        ));
        assert!(!has_code(&lint_decorate("config.request.body;"), "W009"));
    }

    #[test]
    fn w009_not_fired_for_legacy_function_form() {
        let script = "function(request, response) { response.body = 'x'; }";
        assert!(!has_code(&lint_decorate(script), "W009"));
    }

    #[test]
    fn w009_still_fired_for_non_function_junk() {
        // `wait` is JS-only (allow_rhai=false): any non-function, non-empty script warns.
        for script in [
            "response.body = 'x';",
            "response.statusCode = config.statusCode;",
            "var x = appConfig.response;",
        ] {
            assert!(
                has_code(&lint(script), "W009"),
                "W009 should fire for: {script}"
            );
        }
    }

    fn lint_decorate(script: &str) -> LintResult {
        let mut result = LintResult::new();
        validate_javascript_behavior(
            Path::new("test.json"),
            script,
            "loc.decorate",
            &mut result,
            &LintOptions::default(),
            true,
        );
        result
    }

    #[test]
    fn w009_not_fired_for_rhai_decorate() {
        // A Rhai decorate (bare `response.`/`request.` assignment) is valid; the engine routes any
        // non-function, non-config-arrow script to Rhai (issue #257).
        assert!(!has_code(
            &lint_decorate("response.body = \"rhai-\" + request.path;"),
            "W009"
        ));
    }

    #[test]
    fn w009_not_fired_for_any_decorate_script() {
        // decorate accepts JS-function, config-arrow, AND Rhai — the "must be a function
        // expression" nudge never applies to decorate.
        for script in [
            "let x = request.path; response.body = x;",
            "response.statusCode = 503;",
        ] {
            assert!(
                !has_code(&lint_decorate(script), "W009"),
                "W009 fired for decorate: {script}"
            );
        }
    }
}

#[cfg(test)]
mod header_value_tests {
    use super::validate_headers;
    use crate::types::LintResult;
    use serde_json::json;
    use std::path::Path;

    fn lint(headers: serde_json::Value) -> LintResult {
        let mut result = LintResult::new();
        validate_headers(Path::new("test.json"), &headers, "loc", &mut result);
        result
    }

    fn has_code(result: &LintResult, code: &str) -> bool {
        result.issues.iter().any(|i| i.code == code)
    }

    #[test]
    fn array_of_strings_is_valid_header() {
        // Multi-value headers (valid since #238) must not trigger E018.
        let result = lint(json!({ "Set-Cookie": ["sessionId=abc", "theme=dark"] }));
        assert!(!has_code(&result, "E018"));
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn array_with_non_string_errors() {
        // An array with any non-string element (scalar or nested) is not a valid
        // multi-value header — the engine's `OneOrMany` deserialize would reject it.
        for bad in [
            json!(["ok", 42]),
            json!(["ok", null]),
            json!(["ok", { "k": "v" }]),
        ] {
            let result = lint(json!({ "X-Bad": bad }));
            assert!(has_code(&result, "E018"));
        }
    }

    #[test]
    fn empty_array_is_valid_header() {
        // An empty array carries no values; the engine omits it on serialize. No error.
        let result = lint(json!({ "X-Empty": [] }));
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn scalar_non_string_still_errors() {
        let number = lint(json!({ "X-Count": 5 }));
        assert!(has_code(&number, "E019"));
        let boolean = lint(json!({ "X-Flag": true }));
        assert!(has_code(&boolean, "E020"));
    }

    #[test]
    fn string_header_is_valid() {
        let result = lint(json!({ "Content-Type": "text/plain" }));
        assert_eq!(result.errors, 0);
    }
}

// Issue #531: `_rift.fault.tcp` accepts the bare kind string and the probabilistic object form.
#[cfg(test)]
mod tcp_fault_tests {
    use super::validate_tcp_fault;
    use crate::types::LintResult;
    use serde_json::{Value, json};
    use std::path::Path;

    fn lint(tcp: Value) -> LintResult {
        let mut result = LintResult::new();
        validate_tcp_fault(Path::new("test.json"), &tcp, "loc", &mut result);
        result
    }

    fn has_code(result: &LintResult, code: &str) -> bool {
        result.issues.iter().any(|i| i.code == code)
    }

    #[test]
    fn bare_string_known_kind_is_clean() {
        let result = lint(json!("CONNECTION_RESET_BY_PEER"));
        assert_eq!(result.errors, 0);
        assert_eq!(result.warnings, 0);
    }

    #[test]
    fn object_form_with_valid_probability_is_clean() {
        let result = lint(json!({ "probability": 0.1, "type": "reset" }));
        assert_eq!(result.errors, 0);
        assert_eq!(result.warnings, 0);
    }

    #[test]
    fn object_form_missing_probability_errors() {
        let result = lint(json!({ "type": "CONNECTION_RESET_BY_PEER" }));
        assert!(has_code(&result, "E041"));
    }

    #[test]
    fn object_form_probability_out_of_range_errors() {
        assert!(has_code(
            &lint(json!({ "probability": 1.5, "type": "reset" })),
            "E041"
        ));
        assert!(has_code(
            &lint(json!({ "probability": -0.1, "type": "reset" })),
            "E041"
        ));
    }

    #[test]
    fn object_form_missing_type_errors() {
        let result = lint(json!({ "probability": 0.5 }));
        assert!(has_code(&result, "E041"));
    }

    #[test]
    fn unknown_kind_warns_both_forms() {
        assert!(has_code(&lint(json!("NONSENSE")), "W011"));
        assert!(has_code(
            &lint(json!({ "probability": 0.5, "type": "NONSENSE" })),
            "W011"
        ));
    }

    #[test]
    fn non_string_non_object_errors() {
        assert!(has_code(&lint(json!(42)), "E041"));
    }
}

#[cfg(all(test, feature = "javascript"))]
mod js_syntax_tests {
    use super::js_validator::validate_javascript;

    #[test]
    fn non_terminating_js_does_not_hang() {
        // The syntax check must parse, not execute (issue #553). These forms would loop forever
        // under `context.eval` — a bare top-level loop and a self-invoking IIFE, neither wrapped
        // into an uninvoked function expression — but parse instantly. Each runs on a worker
        // thread with a budget so a regression to eval trips the timeout instead of stalling CI;
        // the thread is intentionally detached (never joined) since a regression would hang it.
        for src in ["while (true) {}", "(function(){ while (true) {} })()"] {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(validate_javascript(src));
            });
            let got = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("syntax check must return without executing the script");
            assert!(
                got.is_ok(),
                "an infinite loop is syntactically valid JS: {src}"
            );
        }
    }

    #[test]
    fn valid_function_expression_passes() {
        assert!(validate_javascript("function (args) { return args.length; }").is_ok());
        assert!(validate_javascript("config => { config.response.statusCode = 202; }").is_ok());
    }

    #[test]
    fn invalid_syntax_is_reported() {
        // Unconditionally surfaced — no substring filter that could pass a real error as clean.
        for bad in [
            "function(args){ return",
            "function(args){ if (x { } }",
            "var = 5;",
        ] {
            assert!(
                validate_javascript(bad).is_err(),
                "invalid JS must be reported: {bad}"
            );
        }
    }
}
