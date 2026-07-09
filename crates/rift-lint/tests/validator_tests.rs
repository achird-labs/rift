use rift_lint::{
    LintOptions, LintResult, Severity, lint_directory, lint_file, lint_json, lint_value,
    validate_behavior, validate_imposter, validate_is_response, validate_predicate,
    validate_proxy_response, validate_response, validate_stub,
};
use serde_json::{Value, json};
use std::path::Path;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn path() -> &'static Path {
    Path::new("<test>")
}

fn opts() -> LintOptions {
    LintOptions::default()
}

fn make_imposter(stubs: Value) -> Value {
    json!({
        "port": 3000,
        "protocol": "http",
        "stubs": stubs
    })
}

fn minimal_stub() -> Value {
    json!({
        "responses": [{ "is": { "statusCode": 200 } }]
    })
}

fn has_code(result: &LintResult, code: &str) -> bool {
    result.issues.iter().any(|i| i.code == code)
}

fn codes(result: &LintResult) -> Vec<&str> {
    result.issues.iter().map(|i| i.code.as_str()).collect()
}

// ─── Imposter-level rules ────────────────────────────────────────────────────

#[test]
fn e003_missing_port() {
    let v = json!({ "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E003"), "expected E003, got {:?}", codes(&r));
}

#[test]
fn e003_missing_protocol() {
    let v = json!({ "port": 3000, "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E003"));
}

#[test]
fn e003_missing_stubs() {
    let v = json!({ "port": 3000, "protocol": "http" });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E003"));
}

#[test]
fn e003_not_fired_for_complete_imposter() {
    let v = make_imposter(json!([minimal_stub()]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E003"), "unexpected E003: {:?}", codes(&r));
}

#[test]
fn e004_invalid_protocol() {
    let v = json!({ "port": 3000, "protocol": "smtp", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E004"));
}

#[test]
fn e004_not_fired_for_http() {
    let v = make_imposter(json!([]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E004"));
}

#[test]
fn w010_tcp_protocol_not_supported_by_rift() {
    let v = json!({ "port": 3000, "protocol": "tcp", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E004"), "tcp should not fire E004");
    assert!(
        has_code(&r, "W010"),
        "expected W010 for tcp, got {:?}",
        codes(&r)
    );
}

#[test]
fn e005_port_out_of_range_high() {
    let v = json!({ "port": 70000, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E005"));
}

#[test]
fn e005_port_zero() {
    let v = json!({ "port": 0, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E005"));
}

#[test]
fn w001_privileged_port() {
    let v = json!({ "port": 80, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "W001"));
    assert!(!has_code(&r, "E005"));
}

#[test]
fn w001_not_fired_for_normal_port() {
    let v = make_imposter(json!([]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "W001"));
}

// ─── Stub-level rules ────────────────────────────────────────────────────────

#[test]
fn e006_stub_missing_responses() {
    let stub = json!({ "predicates": [] });
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &serde_json::Value::Null);
    assert!(has_code(&r, "E006"));
}

#[test]
fn w002_stub_empty_responses() {
    let stub = json!({ "responses": [] });
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &serde_json::Value::Null);
    assert!(has_code(&r, "W002"));
}

#[test]
fn w002_not_fired_with_response() {
    let stub = minimal_stub();
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &serde_json::Value::Null);
    assert!(!has_code(&r, "W002"));
}

// ─── Predicate rules ─────────────────────────────────────────────────────────

#[test]
fn e007_predicate_not_object() {
    let pred = json!("equals");
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "stubs[0].predicates[0]", &mut r, &opts());
    assert!(has_code(&r, "E007"));
}

#[test]
fn e008_predicate_no_operator() {
    let pred = json!({ "caseSensitive": true });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "stubs[0].predicates[0]", &mut r, &opts());
    assert!(has_code(&r, "E008"));
}

#[test]
fn e008_not_fired_for_valid_predicate() {
    let pred = json!({ "equals": { "path": "/foo" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "stubs[0].predicates[0]", &mut r, &opts());
    assert!(!has_code(&r, "E008"), "unexpected E008: {:?}", codes(&r));
}

#[test]
fn e009_unknown_predicate_operator() {
    let pred = json!({ "fuzzy": { "path": "/foo" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "E009"));
}

#[test]
fn e034_multiple_operators_in_predicate() {
    let pred = json!({ "equals": { "path": "/a" }, "contains": { "path": "/b" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "E034"));
}

#[test]
fn e010_unbalanced_jsonpath_brackets() {
    let pred = json!({ "equals": { "body": "x" }, "jsonpath": { "selector": "$[0" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "E010"));
}

#[test]
fn e011_jsonpath_missing_selector() {
    let pred = json!({ "equals": { "body": "x" }, "jsonpath": { "ns": {} } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "E011"));
}

#[test]
fn i001_jsonpath_slice_notation() {
    let pred = json!({ "equals": { "body": "x" }, "jsonpath": { "selector": "$[:2]" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "I001"), "expected I001, got {:?}", codes(&r));
}

#[test]
fn e013_invalid_regex_in_matches() {
    let pred = json!({ "matches": { "path": "[invalid" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(has_code(&r, "E013"));
}

#[test]
fn e013_not_fired_for_valid_regex() {
    let pred = json!({ "matches": { "path": "^/api/.*" } });
    let mut r = LintResult::new();
    validate_predicate(path(), &pred, "loc", &mut r, &opts());
    assert!(!has_code(&r, "E013"));
}

// ─── Response-level rules ─────────────────────────────────────────────────────

#[test]
fn e014_response_no_type() {
    let resp = json!({ "behaviors": [] });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(has_code(&r, "E014"));
}

#[test]
fn e014_not_fired_for_is_response() {
    let resp = json!({ "is": { "statusCode": 200 } });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(!has_code(&r, "E014"), "unexpected E014: {:?}", codes(&r));
}

#[test]
fn e014_not_fired_for_rift_response() {
    let resp = json!({ "_rift": { "script": "console.log('hi')" } });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        !has_code(&r, "E014"),
        "E014 should not fire for _rift response, got {:?}",
        codes(&r)
    );
    assert!(
        has_code(&r, "I003"),
        "expected I003 info for _rift, got {:?}",
        codes(&r)
    );
}

#[test]
fn e014_not_fired_for_inject_response() {
    let resp = json!({ "inject": "function(req, state, logger, callback) { callback({statusCode: 200}); }" });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(!has_code(&r, "E014"));
}

#[test]
fn e014_not_fired_for_fault_response() {
    let resp = json!({ "fault": "CONNECTION_RESET_BY_PEER" });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(!has_code(&r, "E014"));
}

#[test]
fn w003_both_is_and_proxy() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "proxy": { "to": "http://example.com" }
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(has_code(&r, "W003"));
}

// ─── Is-response rules ────────────────────────────────────────────────────────

#[test]
fn e015_invalid_status_code() {
    let is_resp = json!({ "statusCode": 999 });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E015"));
}

#[test]
fn e015_not_fired_for_valid_codes() {
    for code in [200u64, 201, 301, 400, 404, 500, 503] {
        let is_resp = json!({ "statusCode": code });
        let mut r = LintResult::new();
        validate_is_response(path(), &is_resp, "loc", &mut r);
        assert!(!has_code(&r, "E015"), "E015 fired for status {code}");
    }
}

#[test]
fn e016_status_code_not_a_number() {
    let is_resp = json!({ "statusCode": "ok" });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E016"));
}

#[test]
fn w004_body_not_json_when_content_type_json() {
    let is_resp = json!({
        "statusCode": 200,
        "headers": { "Content-Type": "application/json" },
        "body": "not json at all {"
    });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "W004"));
}

#[test]
fn w004_not_fired_when_body_is_valid_json() {
    let is_resp = json!({
        "statusCode": 200,
        "headers": { "Content-Type": "application/json" },
        "body": "{\"key\": \"value\"}"
    });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(!has_code(&r, "W004"));
}

// ─── Header rules ─────────────────────────────────────────────────────────────

#[test]
fn e017_empty_header_name() {
    let is_resp = json!({ "headers": { "": "value" } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E017"));
}

#[test]
fn e018_not_fired_for_string_array() {
    // Multi-value headers (array of strings) are valid since #238.
    let is_resp = json!({ "headers": { "Set-Cookie": ["a=1", "b=2"] } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(!has_code(&r, "E018"));
}

#[test]
fn e018_header_array_with_non_string() {
    let is_resp = json!({ "headers": { "X-Custom": ["a", 2] } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E018"));
}

#[test]
fn e019_header_value_is_number() {
    let is_resp = json!({ "headers": { "X-Retry": 3 } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E019"));
}

#[test]
fn e020_header_value_is_boolean() {
    let is_resp = json!({ "headers": { "X-Cached": true } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E020"));
}

#[test]
fn w005_header_value_is_null() {
    let is_resp = json!({ "headers": { "X-Missing": null } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "W005"));
}

#[test]
fn w006_content_length_very_small() {
    let is_resp = json!({ "headers": { "Content-Length": "3" } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "W006"));
}

#[test]
fn w006_not_fired_for_normal_content_length() {
    let is_resp = json!({ "headers": { "Content-Length": "1024" } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(!has_code(&r, "W006"));
}

#[test]
fn e021_headers_not_object() {
    let is_resp = json!({ "headers": ["Content-Type: text/plain"] });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(has_code(&r, "E021"));
}

// ─── Proxy rules ─────────────────────────────────────────────────────────────

#[test]
fn e022_proxy_to_not_http() {
    let proxy = json!({ "to": "ftp://example.com" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "E022"));
}

#[test]
fn e022_not_fired_for_https() {
    let proxy = json!({ "to": "https://example.com" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(!has_code(&r, "E022"));
}

#[test]
fn e023_proxy_to_not_string() {
    let proxy = json!({ "to": 8080 });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "E023"));
}

#[test]
fn e024_proxy_missing_to() {
    let proxy = json!({ "mode": "proxyOnce" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "E024"));
}

#[test]
fn w007_unknown_proxy_mode() {
    let proxy = json!({ "to": "http://example.com", "mode": "mirror" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "W007"));
}

#[test]
fn w007_not_fired_for_known_modes() {
    for mode in ["proxyOnce", "proxyAlways", "proxyTransparent"] {
        let proxy = json!({ "to": "http://example.com", "mode": mode });
        let mut r = LintResult::new();
        validate_proxy_response(path(), &proxy, "loc", &mut r);
        assert!(!has_code(&r, "W007"), "W007 fired for mode {mode}");
    }
}

#[test]
fn i002_proxy_targets_localhost_high_port() {
    let proxy = json!({ "to": "http://localhost:15000" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "I002"), "expected I002, got {:?}", codes(&r));
}

// ─── Behavior rules ───────────────────────────────────────────────────────────

#[test]
fn e025_wait_invalid_type() {
    let behavior = json!({ "wait": true });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E025"));
}

#[test]
fn e025_not_fired_for_numeric_wait() {
    let behavior = json!({ "wait": 500 });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "E025"), "unexpected E025: {:?}", codes(&r));
}

#[test]
fn e025_not_fired_for_wait_range_object() {
    let behavior = json!({ "wait": { "min": 100, "max": 500 } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        !has_code(&r, "E025"),
        "E025 must not fire for {{min,max}} wait object, got {:?}",
        codes(&r)
    );
}

#[test]
fn e025_not_fired_for_js_function_wait() {
    let behavior = json!({ "wait": "function(req) { return 100; }" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "E025"), "unexpected E025: {:?}", codes(&r));
}

#[test]
fn e035_repeat_zero_is_invalid() {
    let behavior = json!({ "repeat": 0 });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        has_code(&r, "E035"),
        "expected E035 for repeat:0, got {:?}",
        codes(&r)
    );
}

#[test]
fn e035_repeat_string_is_invalid() {
    let behavior = json!({ "repeat": "three" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E035"));
}

#[test]
fn e035_not_fired_for_valid_repeat() {
    let behavior = json!({ "repeat": 3 });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "E035"), "unexpected E035: {:?}", codes(&r));
}

#[test]
fn w008_shell_transform_dangerous_command() {
    let behavior = json!({ "shellTransform": "rm -rf /tmp/foo" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "W008"));
}

#[test]
fn w008_not_fired_for_safe_command() {
    let behavior = json!({ "shellTransform": "cat /tmp/response.json" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "W008"));
}

#[test]
fn w009_js_behavior_not_function_expression() {
    // W009 ("should be a function expression") applies to JS-only behaviors. `decorate` also
    // accepts Rhai (issue #257), so the check is scoped to `wait` here.
    let behavior = json!({ "wait": "console.log('hi')" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "W009"));
}

#[test]
fn w009_not_fired_for_function_expression() {
    let behavior = json!({ "decorate": "function(request, response) { response.body = 'ok'; }" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "W009"), "unexpected W009: {:?}", codes(&r));
}

#[test]
fn e026_unbalanced_braces_in_js() {
    let behavior = json!({ "decorate": "function(req, resp) { resp.body = 'hi';" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E026"));
}

#[test]
fn e027_unbalanced_parens_in_js() {
    let behavior = json!({ "decorate": "function(req, resp) { foo(; }" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E027"));
}

// ─── Copy behavior rules ──────────────────────────────────────────────────────

#[test]
fn e029_copy_missing_from() {
    let behavior = json!({ "copy": [{ "into": "body.id" }] });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E029"));
}

#[test]
fn e030_copy_missing_into() {
    let behavior = json!({ "copy": [{ "from": "query.id" }] });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E030"));
}

#[test]
fn copy_valid_not_fired() {
    let behavior = json!({ "copy": [{ "from": "query.id", "into": "body.id" }] });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(!has_code(&r, "E029"));
    assert!(!has_code(&r, "E030"));
}

// ─── Lookup behavior rules ────────────────────────────────────────────────────

#[test]
fn e031_lookup_missing_key() {
    let behavior = json!({ "lookup": { "fromDataSource": { "csv": {} }, "into": "body" } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E031"));
}

#[test]
fn e032_lookup_missing_from_data_source() {
    let behavior = json!({ "lookup": { "key": { "from": "path", "using": { "method": "regex", "selector": "(\\d+)" } }, "into": "body" } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E032"));
}

#[test]
fn e033_lookup_missing_into() {
    let behavior =
        json!({ "lookup": { "key": { "from": "path" }, "fromDataSource": { "csv": {} } } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "E033"));
}

// ─── _behaviors dispatch integration tests ────────────────────────────────────
// These exercise validate_behavior through validate_response using the primary
// Rift format (_behaviors: object) to confirm dispatch is not a dead path.

#[test]
fn e025_via_response_underscore_behaviors_object() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": true }
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        has_code(&r, "E025"),
        "E025 must fire through _behaviors dispatch, got {:?}",
        codes(&r)
    );
}

#[test]
fn e025_not_fired_via_response_for_valid_wait_range() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": { "min": 100, "max": 500 } }
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        !has_code(&r, "E025"),
        "E025 must not fire for {{min,max}} via _behaviors, got {:?}",
        codes(&r)
    );
}

#[test]
fn e035_via_response_underscore_behaviors_object() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "repeat": 0 }
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        has_code(&r, "E035"),
        "E035 must fire through _behaviors dispatch, got {:?}",
        codes(&r)
    );
}

#[test]
fn behaviors_array_format_still_dispatches() {
    // Rift also serializes responses with `behaviors: [...]` (array, no underscore)
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": true }]
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        has_code(&r, "E025"),
        "E025 must fire through behaviors array dispatch, got {:?}",
        codes(&r)
    );
}

#[test]
fn underscore_behaviors_takes_priority_over_behaviors_array() {
    // When both forms are present, _behaviors wins (matches proxy behaviour)
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": 100 },
        "behaviors": [{ "wait": true }]  // invalid, but should not be reached
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &resp,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(
        !has_code(&r, "E025"),
        "_behaviors (valid) should shadow behaviors array, got {:?}",
        codes(&r)
    );
}

// ─── Public API tests ─────────────────────────────────────────────────────────

#[test]
fn lint_json_invalid_json_gives_e002() {
    let result = lint_json("{not json}", "<test>", &opts());
    assert!(
        has_code(&result, "E002"),
        "expected E002, got {:?}",
        codes(&result)
    );
}

#[test]
fn lint_json_valid_imposter_no_errors() {
    let imposter = make_imposter(json!([minimal_stub()]));
    let json_str = serde_json::to_string(&imposter).unwrap();
    let result = lint_json(&json_str, "<test>", &opts());
    assert!(result.is_valid(), "unexpected errors: {:?}", codes(&result));
}

#[test]
fn lint_value_valid_imposter() {
    let imposter = make_imposter(json!([minimal_stub()]));
    let result = lint_value(&imposter, "<test>", &opts());
    assert!(result.is_valid(), "unexpected errors: {:?}", codes(&result));
    assert_eq!(result.files_checked, 1);
}

#[test]
fn lint_result_merge_accumulates() {
    let mut a = LintResult::new();
    a.files_checked = 2;
    a.errors = 1;
    a.warnings = 3;

    let mut b = LintResult::new();
    b.files_checked = 3;
    b.errors = 2;
    b.warnings = 1;

    a.merge(b);

    assert_eq!(a.files_checked, 5);
    assert_eq!(a.errors, 3);
    assert_eq!(a.warnings, 4);
}

#[test]
fn lint_result_is_valid_false_when_errors_present() {
    let mut r = LintResult::new();
    r.errors = 1;
    assert!(!r.is_valid());
}

#[test]
fn lint_result_is_valid_true_when_only_warnings() {
    let mut r = LintResult::new();
    r.warnings = 5;
    assert!(r.is_valid());
}

#[test]
fn lint_directory_reads_json_files() {
    let dir = tempfile::tempdir().unwrap();
    let valid = make_imposter(json!([minimal_stub()]));
    let invalid = json!({ "port": 3001, "protocol": "http" }); // missing stubs

    std::fs::write(
        dir.path().join("valid.json"),
        serde_json::to_string(&valid).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("invalid.json"),
        serde_json::to_string(&invalid).unwrap(),
    )
    .unwrap();
    // Non-JSON files should be ignored
    std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

    let result = lint_directory(dir.path(), &opts());
    assert_eq!(result.files_checked, 2, "should check 2 JSON files");
    assert!(result.has_errors(), "should find error in invalid.json");
}

// ─── Issue #217: config wrapper / multi-imposter formats ─────────────────────

#[test]
fn wrapper_object_validates_each_imposter() {
    // `{"imposters":[...]}` — the form `rift --configfile` accepts — must not be
    // treated as a single imposter (which would spuriously flag E003 on the wrapper).
    let cfg = json!({ "imposters": [ make_imposter(json!([minimal_stub()])) ] }).to_string();
    let r = lint_json(&cfg, "<wrap>", &opts());
    assert!(
        !has_code(&r, "E003"),
        "wrapper object must not produce E003, got {:?}",
        codes(&r)
    );
}

#[test]
fn bare_array_validates_each_imposter() {
    let cfg = json!([make_imposter(json!([minimal_stub()]))]).to_string();
    let r = lint_json(&cfg, "<arr>", &opts());
    assert!(
        !has_code(&r, "E003"),
        "bare array must not produce E003, got {:?}",
        codes(&r)
    );
}

#[test]
fn wrapper_reports_inner_imposter_errors() {
    // The inner imposter has an INVALID protocol → E004. E004 can only come from an
    // imposter that actually has a `protocol` field, so (unlike E003) it can't be
    // produced by the old "validate the wrapper as one imposter" path — proving the
    // array element itself is reached and validated.
    let cfg =
        json!({ "imposters": [ { "port": 3000, "protocol": "smtp", "stubs": [] } ] }).to_string();
    let r = lint_json(&cfg, "<wrap>", &opts());
    assert!(
        has_code(&r, "E004"),
        "invalid protocol inside the wrapper must surface E004, got {:?}",
        codes(&r)
    );
}

#[test]
fn array_validates_every_element_not_just_first() {
    // A bare array whose SECOND element has an invalid protocol must be flagged — guards
    // the `for imposter in arr` loop (a first-element-only bug would pass everything else).
    let cfg = json!([
        make_imposter(json!([minimal_stub()])),
        { "port": 3001, "protocol": "smtp", "stubs": [] }
    ])
    .to_string();
    let r = lint_json(&cfg, "<arr>", &opts());
    assert!(
        has_code(&r, "E004"),
        "invalid protocol on a non-first array element must surface E004, got {:?}",
        codes(&r)
    );
}

#[test]
fn empty_imposters_set_is_clean() {
    // An empty imposter set is valid (matches `rift --configfile`) — no spurious E003.
    for cfg in [
        json!({ "imposters": [] }).to_string(),
        json!([]).to_string(),
    ] {
        let r = lint_json(&cfg, "<empty>", &opts());
        assert!(
            !has_code(&r, "E003"),
            "empty imposter set must not produce E003, got {:?}",
            codes(&r)
        );
    }
}

#[test]
fn single_imposter_still_validates() {
    let cfg = make_imposter(json!([minimal_stub()])).to_string();
    let r = lint_json(&cfg, "<single>", &opts());
    assert!(
        !has_code(&r, "E003"),
        "single imposter must still validate cleanly, got {:?}",
        codes(&r)
    );
}

// ─── Issue #257: W009 must not fire on Rhai decorate (wiring through validate_behavior) ──────

#[test]
fn w009_not_fired_for_rhai_decorate_through_behavior() {
    let behavior = json!({ "decorate": "response.body = \"rhai-\" + request.path;" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        !has_code(&r, "W009"),
        "Rhai decorate must not warn W009: {:?}",
        codes(&r)
    );
}

#[test]
fn w009_still_fired_for_non_function_wait_through_behavior() {
    // wait is JS-only — a non-function wait script still warns (regression guard).
    let behavior = json!({ "wait": "response.body = 'x';" });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(has_code(&r, "W009"), "non-function wait should warn W009");
}

// ─── Issue #356: `_rift.script` file:/ref: validation ────────────────────────

fn rift_script_stub(script: Value) -> Value {
    json!({
        "responses": [{ "_rift": { "script": script } }]
    })
}

#[test]
fn e036_zero_script_sources_is_an_error() {
    let v = make_imposter(json!([rift_script_stub(json!({}))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E036"), "expected E036, got {:?}", codes(&r));
}

#[test]
fn e036_multiple_script_sources_is_an_error() {
    let v = make_imposter(json!([rift_script_stub(
        json!({ "code": "fn should_inject() {}", "file": "x.rhai" })
    )]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E036"), "expected E036, got {:?}", codes(&r));
}

#[test]
fn inline_code_is_accepted_without_e036() {
    let v = make_imposter(json!([rift_script_stub(
        json!({ "code": "fn should_inject() {}" })
    )]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E036"), "got {:?}", codes(&r));
}

#[test]
fn e037_unknown_ref_is_an_error() {
    let v = make_imposter(json!([rift_script_stub(json!({ "ref": "missing" }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E037"), "expected E037, got {:?}", codes(&r));
}

#[test]
fn ref_resolves_against_the_registry_without_e037() {
    let mut v = make_imposter(json!([rift_script_stub(json!({ "ref": "failTwice" }))]));
    v["_rift"] = json!({ "scripts": { "failTwice": { "code": "fn should_inject() {}" } } });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E037"), "got {:?}", codes(&r));
}

#[test]
fn e039_registry_entry_cannot_itself_use_ref() {
    let mut v = make_imposter(json!([minimal_stub()]));
    v["_rift"] = json!({ "scripts": { "a": { "ref": "b" }, "b": { "code": "x" } } });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E039"), "expected E039, got {:?}", codes(&r));
}

#[test]
fn e038_missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = make_imposter(json!([rift_script_stub(
        json!({ "file": "does-not-exist.rhai" })
    )]));
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(has_code(&r, "E038"), "expected E038, got {:?}", codes(&r));
}

#[test]
fn file_resolves_relative_to_the_config_and_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("script.rhai"), "fn should_inject() {}").unwrap();
    let cfg = make_imposter(json!([rift_script_stub(json!({ "file": "script.rhai" }))]));
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(!has_code(&r, "E038"), "got {:?}", codes(&r));
    assert!(!has_code(&r, "E036"), "got {:?}", codes(&r));
}

#[test]
fn ref_to_file_backed_registry_entry_resolves_relative_to_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fail-twice.rhai"), "fn should_inject() {}").unwrap();
    let mut cfg = make_imposter(json!([rift_script_stub(json!({ "ref": "failTwice" }))]));
    cfg["_rift"] = json!({ "scripts": { "failTwice": { "file": "fail-twice.rhai" } } });
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(!has_code(&r, "E037"), "got {:?}", codes(&r));
    assert!(!has_code(&r, "E038"), "got {:?}", codes(&r));
}

#[cfg(feature = "javascript")]
#[test]
fn e040_invalid_javascript_file_syntax_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.js"), "function should_inject( {").unwrap();
    let cfg = make_imposter(json!([rift_script_stub(json!({ "file": "bad.js" }))]));
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(has_code(&r, "E040"), "expected E040, got {:?}", codes(&r));
}

// ─── Issue #357 Item 5: E041 deprecation lint for the v1 `should_inject` wrapper ──────

#[test]
fn e041_fires_for_v1_should_inject_wrapper_rhai() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn should_inject(request, flow_store) { #{ inject: false } }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E041"), "expected E041, got {:?}", codes(&r));
}

#[test]
fn e041_fires_for_v1_should_inject_wrapper_lua() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "engine": "lua",
        "code": "function should_inject(request, flow_store) return { inject = false } end"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E041"), "expected E041, got {:?}", codes(&r));
}

#[test]
fn e041_fires_for_v1_should_inject_wrapper_js() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "engine": "javascript",
        "code": "function should_inject(request, flow_store) { return { inject: false }; }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E041"), "expected E041, got {:?}", codes(&r));
}

#[test]
fn e041_does_not_fire_for_v2_named_respond() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E041"), "got {:?}", codes(&r));
}

#[test]
fn e041_does_not_fire_for_v2_bare_expression() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "if ctx.request.method == \"POST\" { http(503) } else { pass() }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E041"), "got {:?}", codes(&r));
}

#[test]
fn e041_does_not_fire_for_should_inject_only_in_a_comment() {
    // The lint anchors on a function DECLARATION (`fn`/`function should_inject`), so a mere
    // mention of the word in a comment/string is not a v1 wrapper and must not warn.
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "// migrated away from should_inject\nfn respond(ctx) { http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E041"), "got {:?}", codes(&r));
}

#[test]
fn e041_is_a_warning_not_an_error() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn should_inject(request, flow_store) { #{ inject: false } }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "E041")
        .expect("E041 must fire");
    assert_eq!(
        issue.severity,
        Severity::Warning,
        "E041 must be a deprecation warning, not a hard error"
    );
}

// ─── Issue #358: E042 — ctx.state used without _rift.flowState ──────────────

#[test]
fn e042_fires_for_ctx_state_without_flow_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { let n = ctx.state.incr(\"attempts\"); http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E042"), "expected E042, got {:?}", codes(&r));
}

#[test]
fn e042_fires_for_v1_flow_store_without_flow_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn should_inject(request, flow_store) { flow_store.increment(\"f\", \"k\"); #{ inject: false } }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E042"), "expected E042, got {:?}", codes(&r));
}

#[test]
fn e042_does_not_fire_when_flow_state_is_configured() {
    let mut v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { let n = ctx.state.incr(\"attempts\"); http(200) }"
    }))]));
    v["_rift"] = json!({ "flowState": { "backend": "inmemory" } });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "E042"),
        "flowState is configured, E042 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn e042_does_not_fire_for_scripts_that_never_touch_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "E042"),
        "no ctx.state/flow_store usage, E042 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn e042_does_not_fire_for_non_script_imposters() {
    let v = make_imposter(json!([minimal_stub()]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "E042"), "got {:?}", codes(&r));
}

#[test]
fn e042_resolves_file_backed_scripts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("stateful.rhai"),
        "fn respond(ctx) { ctx.state.incr(\"n\"); http(200) }",
    )
    .unwrap();
    let cfg = make_imposter(json!([rift_script_stub(
        json!({ "file": "stateful.rhai" })
    )]));
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(has_code(&r, "E042"), "expected E042, got {:?}", codes(&r));
}

#[test]
fn e042_is_a_warning_not_an_error() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { ctx.state.incr(\"n\"); http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "E042")
        .expect("E042 must fire");
    assert_eq!(
        issue.severity,
        Severity::Warning,
        "E042 must be a hint, not a hard error"
    );
}
