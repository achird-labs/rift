//! Core validation logic for imposter configurations.

use crate::types::{LintIssue, LintOptions, LintResult};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// JavaScript syntax validator using boa_engine.
#[cfg(feature = "javascript")]
mod js_validator {
    use boa_engine::{Context, Source};

    pub fn validate_javascript(script: &str) -> Result<(), String> {
        let mut context = Context::default();

        // Mountebank uses function expressions that need to be wrapped
        let script_trimmed = script.trim();
        let wrapped =
            if script_trimmed.starts_with("function") && !script_trimmed.contains("function ") {
                format!("var __fn = ({script_trimmed})")
            } else {
                script_trimmed.to_string()
            };

        match context.eval(Source::from_bytes(&wrapped)) {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("SyntaxError") || err_str.contains("unexpected") {
                    Err(err_str)
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[cfg(not(feature = "javascript"))]
mod js_validator {
    #[allow(dead_code)]
    pub fn validate_javascript(_script: &str) -> Result<(), String> {
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

    if let Some(stubs) = imposter.get("stubs").and_then(|v| v.as_array()) {
        for (idx, stub) in stubs.iter().enumerate() {
            validate_stub(file, stub, idx, result, options);
        }
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

/// Validate a single stub.
pub fn validate_stub(
    file: &Path,
    stub: &Value,
    idx: usize,
    result: &mut LintResult,
    options: &LintOptions,
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

    let pred_obj = match predicate.as_object() {
        Some(obj) => obj,
        None => {
            result.add_issue(
                LintIssue::error("E007", "Predicate must be an object", file.to_path_buf())
                    .with_location(location),
            );
            return;
        }
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
        let suggestion =
            format!("Split into separate predicates, combined with and, like: {and_predicate} (or just {sub_predicates} if at the top level)");
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
            if let Some(pattern_str) = pattern.as_str() {
                if let Err(e) = Regex::new(pattern_str) {
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
}

/// Validate a response object.
pub fn validate_response(
    file: &Path,
    response: &Value,
    location: &str,
    result: &mut LintResult,
    options: &LintOptions,
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

    if let Some(proxy) = response.get("proxy") {
        if !proxy.is_null() {
            validate_proxy_response(file, proxy, &format!("{location}.proxy"), result);
        }
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
    if let Some(body) = is_response.get("body") {
        if let Some(headers) = is_response.get("headers").and_then(|h| h.as_object()) {
            let content_type = headers
                .iter()
                .find(|(k, _)| k.to_lowercase() == "content-type")
                .and_then(|(_, v)| v.as_str());

            if content_type
                .map(|ct| ct.contains("application/json"))
                .unwrap_or(false)
            {
                if let Some(body_str) = body.as_str() {
                    if serde_json::from_str::<Value>(body_str).is_err() {
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
                .with_suggestion(format!("Change to: \"{name}\": \"{}\"", value)),
            );
        } else if value.is_boolean() {
            result.add_issue(
                LintIssue::error(
                    "E020",
                    format!("Header '{name}' value is a boolean, must be a string"),
                    file.to_path_buf(),
                )
                .with_location(format!("{location}.{name}"))
                .with_suggestion(format!("Change to: \"{name}\": \"{}\"", value)),
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

        if name.to_lowercase() == "content-length" {
            if let Some(len_str) = value.as_str() {
                if let Ok(len) = len_str.parse::<u64>() {
                    if len < 10 {
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
                if let Some(captures) = port_re.captures(url) {
                    if let Ok(port) = captures[1].parse::<u16>() {
                        if port > 10000 {
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

    if let Some(decorate) = obj.get("decorate") {
        if let Some(script) = decorate.as_str() {
            validate_javascript_behavior(
                file,
                script,
                &format!("{location}.decorate"),
                result,
                options,
            );
        }
    }

    if let Some(shell) = obj.get("shellTransform") {
        if let Some(cmd) = shell.as_str() {
            let dangerous_patterns = ["rm ", "rm -", "sudo ", "chmod ", "dd ", "> /dev/"];
            for pattern in dangerous_patterns {
                if cmd.contains(pattern) {
                    result.add_issue(
                        LintIssue::warning(
                            "W008",
                            format!(
                                "shellTransform contains potentially dangerous command: {pattern}"
                            ),
                            file.to_path_buf(),
                        )
                        .with_location(format!("{location}.shellTransform"))
                        .with_suggestion("Review this command for safety"),
                    );
                }
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

/// Validate JavaScript in a behavior.
fn validate_javascript_behavior(
    file: &Path,
    script: &str,
    location: &str,
    result: &mut LintResult,
    _options: &LintOptions,
) {
    let script_trimmed = script.trim();

    if !script_trimmed.starts_with("function") && !script_trimmed.is_empty() {
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
