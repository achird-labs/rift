use rift_lint::{
    LintOptions, LintResult, Severity, imposters_in, lint_directory, lint_file, lint_json,
    lint_value, validate_behavior, validate_imposter, validate_is_response, validate_predicate,
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

/// Issue #1104: the engine auto-assigns `port: 0` like an absent port, so the lint must not call it
/// "out of range" — it is refused for the same reason an absent port is (E003): a config file pins
/// its ports.
#[test]
fn e005_for_port_zero_says_it_is_auto_assigned() {
    let v = json!({ "port": 0, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let e005: Vec<_> = r.issues.iter().filter(|i| i.code == "E005").collect();
    assert_eq!(e005.len(), 1, "{:?}", codes(&r));
    assert!(
        e005[0].message.contains("auto-assign"),
        "{}",
        e005[0].message
    );
    assert!(
        !e005[0].message.contains("out of valid range"),
        "{}",
        e005[0].message
    );

    let v = json!({ "port": 70000, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let e005: Vec<_> = r.issues.iter().filter(|i| i.code == "E005").collect();
    assert!(
        e005[0].message.contains("out of valid range"),
        "{}",
        e005[0].message
    );
}

fn port_issue_codes(port: Value) -> Vec<String> {
    let v = json!({ "port": port, "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    r.issues.into_iter().map(|i| i.code).collect()
}

// Issue #1088: the engine reads `port` as `Option<u16>`, and serde's `u16` visitor has no float
// arm, so every one of these is refused at load (`invalid type`/`invalid value` `…, expected u16`). An integral
// float (`3000.0`, `3e3`) is refused exactly like a fractional one.
#[test]
fn e047_port_not_an_integer() {
    for bad in [
        json!(3000.5),
        json!(3000.0),
        json!(3e3),
        json!("3000"),
        json!(-1),
        json!(true),
        json!([3000]),
        json!({}),
    ] {
        assert_eq!(
            port_issue_codes(bad.clone()),
            vec!["E047".to_string()],
            "port {bad} must report exactly E047"
        );
    }
}

#[test]
fn e047_message_and_location_name_the_value_as_written() {
    let v = json!({ "port": "3000", "protocol": "http", "stubs": [] });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "E047")
        .expect("E047 reported");
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(issue.location.as_deref(), Some("port"));
    assert_eq!(
        issue.message,
        "Port must be an integer from 1 to 65535, got \"3000\""
    );
}

#[test]
fn e047_leaves_integer_ports_to_the_range_checks() {
    assert_eq!(port_issue_codes(json!(3000)), Vec::<String>::new());
    assert_eq!(port_issue_codes(json!(65535)), Vec::<String>::new());
    assert_eq!(port_issue_codes(json!(65536)), vec!["E005".to_string()]);
    assert_eq!(port_issue_codes(json!(70000)), vec!["E005".to_string()]);
    assert_eq!(port_issue_codes(json!(0)), vec!["E005".to_string()]);
    assert_eq!(port_issue_codes(json!(80)), vec!["W001".to_string()]);
}

// Each imposter in a wrapper is checked on its own: one bad port is one E047, not one per imposter.
#[test]
fn e047_fires_once_for_the_one_bad_port_in_a_wrapper() {
    let doc = json!({ "imposters": [
        { "port": 3000, "protocol": "http", "stubs": [] },
        { "port": "3001", "protocol": "http", "stubs": [] },
        { "port": 3002, "protocol": "http", "stubs": [] }
    ]});
    let r = lint_value(&doc, "<test>", &opts());
    assert_eq!(codes(&r), vec!["E047"], "got {:?}", codes(&r));
    assert!(r.issues[0].message.ends_with("got \"3001\""));
}

// `"port": null` loads: `Option<u16>` reads it as absent and the engine auto-assigns. It is
// reported as the missing field it is to the engine, never as a type error.
#[test]
fn e003_fires_for_null_required_fields() {
    assert_eq!(port_issue_codes(Value::Null), vec!["E003".to_string()]);
    let mut r = LintResult::new();
    validate_imposter(
        path(),
        &json!({ "port": null, "protocol": "http", "stubs": [] }),
        &mut r,
        &opts(),
    );
    assert_eq!(r.issues[0].message, "Missing required field: port");

    for field in ["protocol", "stubs"] {
        let mut v = json!({ "port": 3000, "protocol": "http", "stubs": [] });
        v[field] = Value::Null;
        let mut r = LintResult::new();
        validate_imposter(path(), &v, &mut r, &opts());
        assert_eq!(
            codes(&r),
            vec!["E003"],
            "{field}: null must report E003, got {:?}",
            codes(&r)
        );
        assert_eq!(
            r.issues[0].message,
            format!("Missing required field: {field}")
        );
    }
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

// Issue #1148: an inverted range is not a *shape* error — `is_valid_wait_range` holds — but the
// engine now refuses the imposter, and "the engine refuses it" is the bar for an E-rule. Folded
// into E025 rather than given a new code, the treatment #1090 gave the fractional case.
#[test]
fn e025_fired_for_an_inverted_wait_range() {
    let behavior = json!({ "wait": { "min": 100, "max": 7 } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        has_code(&r, "E025"),
        "E025 must fire for an inverted wait range, got {:?}",
        codes(&r)
    );
}

#[test]
fn e025_not_fired_for_an_equal_bound_wait_range() {
    let behavior = json!({ "wait": { "min": 250, "max": 250 } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        !has_code(&r, "E025"),
        "equal bounds are a valid inclusive range, got {:?}",
        codes(&r)
    );
}

// Issue #1148: the engine refuses an inverted range **wherever it sits**, so the linter has to
// match it — otherwise a file lints clean in CI and then 400s on deploy, which is the exact thing
// an E-rule exists to prevent. These three shapes all passed the linter before the fix.
#[test]
fn e025_fired_for_an_inverted_range_in_a_non_last_behaviors_element() {
    let response = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": { "min": 100, "max": 7 } }, { "wait": 50 }]
    });
    let mut r = LintResult::new();
    validate_response(path(), &response, "loc", &mut r, &opts(), &json!({}));
    assert!(
        has_code(&r, "E025"),
        "the losing element still refuses in the engine, got {:?}",
        codes(&r)
    );
}

#[test]
fn e025_fired_for_an_inverted_range_in_a_behaviors_block_shadowed_by_underscore_behaviors() {
    let response = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": 100 },
        "behaviors": [{ "wait": { "min": 100, "max": 7 } }]
    });
    let mut r = LintResult::new();
    validate_response(path(), &response, "loc", &mut r, &opts(), &json!({}));
    assert!(
        has_code(&r, "E025"),
        "the engine validates both blocks even though it evaluates only one, got {:?}",
        codes(&r)
    );
}

// Each finding must be reported once: the winning block is reported by the normal precedence-aware
// path, the shadowed one by the sweep, and neither should double up.
#[test]
fn e025_is_reported_once_for_a_single_inverted_range() {
    let response = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "wait": { "min": 100, "max": 7 } }
    });
    let mut r = LintResult::new();
    validate_response(path(), &response, "loc", &mut r, &opts(), &json!({}));
    let hits = codes(&r).iter().filter(|c| **c == "E025").count();
    assert_eq!(hits, 1, "expected exactly one E025, got {:?}", codes(&r));
}

#[test]
fn e025_fired_for_an_inverted_delay_range() {
    let stub = json!({
        "predicates": [],
        "delayRange": [{ "min": 100, "max": 7 }],
        "responses": [{ "is": { "statusCode": 200 } }]
    });
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &json!({}));
    assert!(
        has_code(&r, "E025"),
        "E025 must fire for an inverted delayRange, got {:?}",
        codes(&r)
    );
}

// The bounds may be numeric strings, as the engine's `de_u64_or_string` accepts.
#[test]
fn e025_fired_for_an_inverted_delay_range_written_as_strings() {
    let stub = json!({
        "predicates": [],
        "delayRange": [{ "min": "100", "max": "7" }],
        "responses": [{ "is": { "statusCode": 200 } }]
    });
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &json!({}));
    assert!(has_code(&r, "E025"), "got {:?}", codes(&r));
}

#[test]
fn e025_not_fired_for_a_valid_delay_range() {
    let stub = json!({
        "predicates": [],
        "delayRange": [{ "min": 0, "max": 0 }, { "min": 10, "max": 100 }],
        "responses": [{ "is": { "statusCode": 200 } }]
    });
    let mut r = LintResult::new();
    validate_stub(path(), &stub, 0, &mut r, &opts(), &json!({}));
    assert!(!has_code(&r, "E025"), "got {:?}", codes(&r));
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

// AC 608-5 (#608): the object-form wait is valid — lint used to mirror the engine enum and so
// flagged the repo's own shipped example (`examples/latency-testing.json`) as an error.
#[test]
fn e025_not_fired_for_inject_object_wait() {
    let behavior = json!({ "wait": { "inject": "function() { return 100; }" } });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(
        !has_code(&r, "E025"),
        "E025 must not fire for the documented {{inject}} wait object, got {:?}",
        codes(&r)
    );
}

// Accepting the object form must not make E025 a rubber stamp for any object.
#[test]
fn e025_still_fires_for_malformed_wait_objects() {
    for bad in [
        json!({ "wait": { "bogus": true } }),
        json!({ "wait": { "inject": 42 } }),
        json!({ "wait": { "min": 100 } }),
    ] {
        let mut r = LintResult::new();
        validate_behavior(path(), &bad, "loc", &mut r, &opts());
        assert!(has_code(&r, "E025"), "E025 must still fire for {bad}");
    }
}

// Issue #1088: `WaitBehavior::Fixed` is a `u64`, so a fractional or negative number matches no
// variant of the untagged enum. The engine then ignores every parsed behavior in the block (all but
// `repeat`, which it reads separately) with only a log line, so the lint must not pass it as "a number".
#[test]
fn e025_fires_for_a_fractional_or_negative_wait() {
    for bad in [json!(500.5), json!(500.0), json!(-1)] {
        let behavior = json!({ "wait": bad });
        let mut r = LintResult::new();
        validate_behavior(path(), &behavior, "loc", &mut r, &opts());
        assert!(has_code(&r, "E025"), "E025 must fire for wait {bad}");
    }
}

#[test]
fn e025_not_fired_for_a_zero_wait() {
    let behavior = json!({ "wait": 0 });
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

// Issue #1093: `null` for a behavior key is the key absent, as it is to the engine.
#[test]
fn null_behavior_keys_are_absent() {
    let behavior = json!({
        "wait": null, "repeat": null, "decorate": null,
        "shellTransform": null, "copy": null, "lookup": null
    });
    let mut r = LintResult::new();
    validate_behavior(path(), &behavior, "loc", &mut r, &opts());
    assert!(r.issues.is_empty(), "got {:?}", codes(&r));

    for (bad, code) in [
        (json!({ "wait": true }), "E025"),
        (json!({ "repeat": "3" }), "E035"),
    ] {
        let mut r = LintResult::new();
        validate_behavior(path(), &bad, "loc", &mut r, &opts());
        assert_eq!(codes(&r), vec![code], "{bad}");
    }
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

/// Issue #1099: every behavior finding on `resp`, as `(code, location)`, validated the way
/// `validate_response` reaches it.
fn behavior_findings(resp: &Value) -> Vec<(String, String)> {
    let mut r = LintResult::new();
    validate_response(path(), resp, "loc", &mut r, &opts(), &Value::Null);
    r.issues
        .iter()
        .filter(|i| {
            i.location
                .as_deref()
                .is_some_and(|l| l.contains("behaviors"))
        })
        .map(|i| {
            let location = i.location.clone().expect("filtered to Some above");
            (i.code.clone(), location)
        })
        .collect()
}

fn finding(code: &str, location: &str) -> (String, String) {
    (code.to_string(), location.to_string())
}

/// The engine reads `"_behaviors": null` as absent and falls back to the array, so the lint must
/// too; the null used to shadow the array and the block linted clean.
#[test]
fn null_underscore_behaviors_falls_back_to_the_array() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": null,
        "behaviors": [{ "wait": true }]
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![finding("E025", "loc.behaviors[0].wait")]
    );
}

/// The engine folds the array last-write-wins per key, so an earlier bad value that a later one
/// overrides is never read.
#[test]
fn behaviors_array_is_merged_last_write_wins() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": true }, { "wait": 5 }]
    });
    assert_eq!(behavior_findings(&resp), vec![]);
}

#[test]
fn behaviors_array_reports_the_winning_element() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": 5 }, { "wait": 7 }, { "wait": true }]
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![finding("E025", "loc.behaviors[2].wait")]
    );
}

#[test]
fn behaviors_array_disjoint_keys_are_each_validated_at_their_element() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": true }, 5, { "repeat": 0 }]
    });
    // The skipped `5` is itself E048 (issue #1101) and does not shift the indexes after it.
    assert_eq!(
        behavior_findings(&resp),
        vec![
            finding("E025", "loc.behaviors[0].wait"),
            finding("E048", "loc.behaviors[1]"),
            finding("E035", "loc.behaviors[2].repeat"),
        ]
    );
}

/// A later `null` makes the key absent in the merged block, exactly as it does in `_behaviors`.
#[test]
fn behaviors_array_later_null_makes_the_key_absent() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [{ "wait": true }, { "wait": null }]
    });
    assert_eq!(behavior_findings(&resp), vec![]);
}

/// The engine passes an object-valued `behaviors` through unchanged; the lint used to skip it.
#[test]
fn behaviors_object_form_is_validated() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": { "wait": true, "repeat": 0 }
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![
            finding("E025", "loc.behaviors.wait"),
            finding("E035", "loc.behaviors.repeat"),
        ]
    );
}

#[test]
fn a_live_underscore_behaviors_still_shadows_a_bad_array_at_its_own_location() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": { "repeat": 0 },
        "behaviors": [{ "wait": true }]
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![finding("E035", "loc._behaviors.repeat")]
    );
}

/// Every behavior key follows the fold, not just `wait`: `copy` from the last element that sets it,
/// and `lookup` and `shellTransform` from the elements that set them once.
#[test]
fn behaviors_array_folds_copy_lookup_and_shell_transform_too() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "behaviors": [
            { "copy": [{ "into": "a" }] },
            { "lookup": { "key": "k", "into": "y" } },
            { "shellTransform": "sudo reboot" },
            { "copy": [{ "from": "body", "into": "b" }, { "into": "c" }] }
        ]
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![
            finding("E032", "loc.behaviors[1].lookup"),
            finding("W008", "loc.behaviors[2].shellTransform"),
            finding("E029", "loc.behaviors[3].copy[1]"),
        ]
    );
}

/// The engine refuses a non-null, non-object `_behaviors` (issue #1101), so it is reported once as
/// E048 — and the `behaviors` array beside it is not linted as if it were the fallback.
#[test]
fn a_non_object_underscore_behaviors_is_e048_and_does_not_fall_back_to_the_array() {
    let resp = json!({
        "is": { "statusCode": 200 },
        "_behaviors": "not-an-object",
        "behaviors": [{ "wait": true }]
    });
    assert_eq!(
        behavior_findings(&resp),
        vec![("E048".to_string(), "loc._behaviors".to_string())]
    );
}

/// Issue #1101: the positional array the engine used to read as a shellTransform.
#[test]
fn an_array_underscore_behaviors_is_e048() {
    for block in [json!([null, null, null, null, "echo pwned"]), json!([])] {
        let resp = json!({ "is": {}, "_behaviors": block });
        assert_eq!(
            behavior_findings(&resp),
            vec![("E048".to_string(), "loc._behaviors".to_string())],
            "for {resp}"
        );
    }
}

#[test]
fn a_scalar_behaviors_is_e048() {
    for block in [json!("x"), json!(5), json!(true)] {
        let resp = json!({ "is": {}, "behaviors": block });
        assert_eq!(
            behavior_findings(&resp),
            vec![("E048".to_string(), "loc.behaviors".to_string())],
            "for {resp}"
        );
    }
}

/// The engine skips a non-object element of a `behaviors` array, so the author's entry does nothing.
/// A `null` element is absent, like a `null` key, and is not reported.
#[test]
fn a_non_object_element_of_a_behaviors_array_is_e048_at_its_index() {
    let resp = json!({ "is": {}, "behaviors": [5, { "wait": 10 }, null, "x"] });
    assert_eq!(
        behavior_findings(&resp),
        vec![
            ("E048".to_string(), "loc.behaviors[0]".to_string()),
            ("E048".to_string(), "loc.behaviors[3]".to_string()),
        ]
    );
}

#[test]
fn e048_names_the_shape_and_what_the_engine_does_with_it() {
    let messages = |resp: Value| {
        let mut r = LintResult::new();
        validate_response(path(), &resp, "loc", &mut r, &opts(), &Value::Null);
        r.issues
            .into_iter()
            .filter(|i| i.code == "E048")
            .map(|i| i.message)
            .collect::<Vec<_>>()
    };
    let refused = messages(json!({ "is": {}, "_behaviors": [1] }));
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("`_behaviors`") && refused[0].contains("array"),
        "{refused:?}"
    );
    assert!(refused[0].contains("refuses"), "{refused:?}");

    let skipped = messages(json!({ "is": {}, "behaviors": ["x"] }));
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].contains("skips"), "{skipped:?}");
}

#[test]
fn empty_or_null_behaviors_forms_report_nothing() {
    for resp in [
        json!({ "is": {}, "behaviors": [] }),
        json!({ "is": {}, "behaviors": null }),
        json!({ "is": {}, "_behaviors": null, "behaviors": null }),
        json!({ "is": {}, "_behaviors": {}, "behaviors": {} }),
    ] {
        assert_eq!(behavior_findings(&resp), vec![], "for {resp}");
    }
}

// ─── Templated documents (issue #1108) ────────────────────────────────────────
//
// The engine renders `<% %>` tags before it parses a `--configfile`, so the lint does too, with the
// same code (`rift-ejs`). These use a variable no environment sets, so the result does not depend on
// the machine running the tests.

const UNSET: &str = "RIFT_LINT_TEST_1108_NEVER_SET";

#[test]
fn a_templated_port_with_a_default_lints_clean() {
    let text = format!(
        r#"{{"port": <%= process.env.{UNSET} || '4545' %>, "protocol": "http", "stubs": []}}"#
    );
    let r = lint_json(&text, "imposters.json", &opts());
    assert!(
        r.issues.is_empty(),
        "the documented form must not be E001: {:?}",
        codes(&r)
    );
}

#[test]
fn a_templated_document_is_validated_as_rendered() {
    let text = format!(
        r#"{{"port": <%= process.env.{UNSET} || '70000' %>, "protocol": "http", "stubs": []}}"#
    );
    let r = lint_json(&text, "imposters.json", &opts());
    assert_eq!(codes(&r), vec!["E005"]);
}

#[test]
fn an_unsupported_tag_is_e049_with_the_engine_message() {
    let r = lint_json(
        r#"{"port": 4545, "protocol": "http", "stubs": [{"responses": [{"is": {"body": "<% for (x) %>"}}]}]}"#,
        "imposters.json",
        &opts(),
    );
    assert_eq!(codes(&r), vec!["E049"]);
    assert!(
        r.issues[0].message.starts_with(
            "unsupported EJS tag `<% for (x) %>` at imposters.json:1, so the file was not loaded."
        ),
        "{}",
        r.issues[0].message
    );
    assert_eq!(r.errors, 1);
}

#[test]
fn a_missing_include_is_e049() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(&path, r#"{"imposters": [<% include 'missing.json' %>]}"#).expect("write");
    let r = lint_file(&path, &opts());
    assert_eq!(codes(&r), vec!["E049"]);
    assert_eq!(r.errors, 1);
    assert!(
        r.issues[0].message.contains("missing.json"),
        "{}",
        r.issues[0].message
    );
}

#[test]
fn an_unset_variable_without_a_default_is_w013() {
    let text = format!(
        r#"{{"port": 4545, "protocol": "http", "stubs": [{{"responses": [{{"is": {{"body": "<%= process.env.{UNSET} %>"}}}}]}}]}}"#
    );
    let r = lint_json(&text, "imposters.json", &opts());
    assert_eq!(codes(&r), vec!["W013"]);
    assert!(
        r.issues[0].message.contains(UNSET),
        "{}",
        r.issues[0].message
    );
    assert!(
        r.issues[0].message.contains("imposters.json:1"),
        "{}",
        r.issues[0].message
    );
    assert_eq!((r.errors, r.warnings), (0, 1));
}

// Issue #1116: a set variable whose value is not valid Unicode is not "unset", and it is ignored
// even when the tag has a default, so it is reported either way with its own wording.
#[cfg(unix)]
#[test]
fn a_non_unicode_variable_is_w013_with_its_own_wording() {
    use std::os::unix::ffi::OsStrExt;
    const NOT_UNICODE: &str = "RIFT_LINT_TEST_1116_NOT_UNICODE";
    // Safety: a name no other test reads, set once and never removed.
    unsafe { std::env::set_var(NOT_UNICODE, std::ffi::OsStr::from_bytes(b"\xff")) };
    let text = format!(
        r#"{{"port": 4545, "protocol": "http", "stubs": [{{"responses": [{{"is": {{"body": "<%= process.env.{NOT_UNICODE} || 'x' %>"}}}}]}}]}}"#
    );
    let r = lint_json(&text, "imposters.json", &opts());
    assert_eq!(codes(&r), vec!["W013"]);
    assert!(
        r.issues[0].message.contains("not valid Unicode")
            && !r.issues[0].message.contains("is unset"),
        "{}",
        r.issues[0].message
    );
    assert!(
        r.issues[0]
            .suggestion
            .as_deref()
            .is_some_and(|s| s.contains("valid Unicode")),
        "{:?}",
        r.issues[0].suggestion
    );
}

#[test]
fn an_included_file_is_linted_as_part_of_the_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("imposter.json"),
        r#"{"port": 70000, "protocol": "http", "stubs": []}"#,
    )
    .expect("write include");
    let path = dir.path().join("imposters.json");
    std::fs::write(&path, r#"{"imposters": [<% include 'imposter.json' %>]}"#).expect("write");
    let r = lint_file(&path, &opts());
    assert!(
        has_code(&r, "E005"),
        "the included imposter is linted: {:?}",
        codes(&r)
    );
    assert!(!has_code(&r, "E001"), "{:?}", codes(&r));
}

// Includes and substitutions move lines and columns, so a finding that names one says it counts in
// the rendered text rather than pointing at the wrong place in the file.
#[test]
fn a_position_in_a_rendered_document_says_so() {
    let broken = format!(
        r#"{{"port": <%= process.env.{UNSET} || '4545' %>,, "protocol": "http", "stubs": []}}"#
    );
    let r = lint_json(&broken, "imposters.json", &opts());
    assert_eq!(codes(&r), vec!["E001"]);
    assert!(
        r.issues[0]
            .message
            .ends_with("(line and column are in the rendered document)"),
        "{}",
        r.issues[0].message
    );

    let lossy = format!(
        r#"{{"port": <%= process.env.{UNSET} || '4545' %>, "protocol": "http", "stubs": [{{"responses": [{{"is": {{"body": {{"big": 123456789012345678901234567890}}}}}}]}}]}}"#
    );
    let r = lint_json(&lossy, "imposters.json", &opts());
    let w012: Vec<_> = r.issues.iter().filter(|i| i.code == "W012").collect();
    assert_eq!(w012.len(), 1, "{:?}", codes(&r));
    assert!(
        w012[0]
            .location
            .as_deref()
            .is_some_and(|l| l.ends_with(" of the rendered document")),
        "{:?}",
        w012[0].location
    );

    let plain = r#"{"port": 4545,, "protocol": "http", "stubs": []}"#;
    let r = lint_json(plain, "imposters.json", &opts());
    assert!(
        !r.issues[0].message.contains("rendered"),
        "an untemplated file is not annotated"
    );
}

#[test]
fn a_templated_yaml_document_lints_clean() {
    let text =
        format!("- port: <%= process.env.{UNSET} || '4545' %>\n  protocol: http\n  stubs: []\n");
    let r = rift_lint::lint_yaml(&text, "imposters.yaml", &opts());
    assert!(r.issues.is_empty(), "{:?}", codes(&r));
}

#[test]
fn no_parse_lints_the_text_verbatim() {
    let verbatim = LintOptions { no_parse: true };
    let text = format!(
        r#"{{"port": <%= process.env.{UNSET} || '4545' %>, "protocol": "http", "stubs": []}}"#
    );
    let r = lint_json(&text, "imposters.json", &verbatim);
    assert_eq!(
        codes(&r),
        vec!["E001"],
        "--no-parse loads the tag as text, which is not JSON"
    );

    let literal = r#"{"port": 4545, "protocol": "http", "stubs": [{"responses": [{"is": {"body": "<% literal %>"}}]}]}"#;
    let r = lint_json(literal, "imposters.json", &verbatim);
    assert!(
        r.issues.is_empty(),
        "a literal tag in a string is fine verbatim: {:?}",
        codes(&r)
    );
}

// ─── Public API tests ─────────────────────────────────────────────────────────

/// Issue #1008: this asserted `E002`, encoding the collision rather than the contract — the
/// published table and the CLI both use `E001` for unparsable JSON and reserve `E002` for the
/// port conflict. The library now agrees with them, so the assertion moves with it.
#[test]
fn lint_json_invalid_json_gives_e001() {
    let result = lint_json("{not json}", "<test>", &opts());
    assert!(
        has_code(&result, "E001"),
        "expected E001, got {:?}",
        codes(&result)
    );
    assert!(
        !has_code(&result, "E002"),
        "E002 is the port conflict and must not be reported for a syntax error: {:?}",
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
        json!({ "code": "fn respond() {}", "file": "x.rhai" })
    )]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "E036"), "expected E036, got {:?}", codes(&r));
}

#[test]
fn inline_code_is_accepted_without_e036() {
    let v = make_imposter(json!([rift_script_stub(
        json!({ "code": "fn respond() {}" })
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
    v["_rift"] = json!({ "scripts": { "failTwice": { "code": "fn respond() {}" } } });
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
    std::fs::write(dir.path().join("script.rhai"), "fn respond() {}").unwrap();
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
    std::fs::write(dir.path().join("fail-twice.rhai"), "fn respond() {}").unwrap();
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
    std::fs::write(dir.path().join("bad.js"), "function respnod( {").unwrap();
    let cfg = make_imposter(json!([rift_script_stub(json!({ "file": "bad.js" }))]));
    let config_path = dir.path().join("imposter.json");
    std::fs::write(&config_path, serde_json::to_string(&cfg).unwrap()).unwrap();

    let r = lint_file(&config_path, &opts());
    assert!(has_code(&r, "E040"), "expected E040, got {:?}", codes(&r));
}

// ─── Issue #358: W014 (was E042, #1156) — ctx.state used without _rift.flowState ─

#[test]
fn w014_fires_for_ctx_state_without_flow_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { let n = ctx.state.incr(\"attempts\"); http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "W014"), "expected W014, got {:?}", codes(&r));
}

// W014's textual check also keys on the literal `flow_store` substring (kept for scripts that
// still reference it), independent of whether the function is named `respond` or anything else.
#[test]
fn w014_fires_for_flow_store_text_without_flow_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn legacy(ctx) { flow_store.increment(\"f\", \"k\"); }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "W014"), "expected W014, got {:?}", codes(&r));
}

#[test]
fn w014_does_not_fire_when_flow_state_is_configured() {
    let mut v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { let n = ctx.state.incr(\"attempts\"); http(200) }"
    }))]));
    v["_rift"] = json!({ "flowState": { "backend": "inmemory" } });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "W014"),
        "flowState is configured, W014 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn w014_does_not_fire_for_scripts_that_never_touch_state() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "W014"),
        "no ctx.state/flow_store usage, W014 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn w014_does_not_fire_for_non_script_imposters() {
    let v = make_imposter(json!([minimal_stub()]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(!has_code(&r, "W014"), "got {:?}", codes(&r));
}

#[test]
fn w014_resolves_file_backed_scripts() {
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
    assert!(has_code(&r, "W014"), "expected W014, got {:?}", codes(&r));
}

// ─── Issue #969: W014 also fires for `_rift.stateOps` without `_rift.flowState` ─────────────

fn state_ops_stub(ops: Value) -> Value {
    json!({
        "responses": [{ "is": { "statusCode": 200 }, "_rift": { "stateOps": ops } }]
    })
}

#[test]
fn w014_fires_for_state_ops_without_flow_state() {
    let v = make_imposter(json!([state_ops_stub(json!([
        { "op": "increment", "key": "hits" }
    ]))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(has_code(&r, "W014"), "expected W014, got {:?}", codes(&r));
}

#[test]
fn w014_does_not_fire_for_empty_state_ops() {
    let v = make_imposter(json!([state_ops_stub(json!([]))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "W014"),
        "an empty stateOps array touches nothing, W014 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn w014_does_not_fire_for_state_ops_when_flow_state_is_configured() {
    let mut v = make_imposter(json!([state_ops_stub(json!([
        { "op": "increment", "key": "hits" }
    ]))]));
    v["_rift"] = json!({ "flowState": { "backend": "inmemory" } });
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    assert!(
        !has_code(&r, "W014"),
        "flowState is configured, W014 must not fire, got {:?}",
        codes(&r)
    );
}

#[test]
fn w014_is_a_warning_not_an_error() {
    let v = make_imposter(json!([rift_script_stub(json!({
        "code": "fn respond(ctx) { ctx.state.incr(\"n\"); http(200) }"
    }))]));
    let mut r = LintResult::new();
    validate_imposter(path(), &v, &mut r, &opts());
    let issue = r
        .issues
        .iter()
        .find(|i| i.code == "W014")
        .expect("W014 must fire");
    assert_eq!(
        issue.severity,
        Severity::Warning,
        "W014 must be a hint, not a hard error"
    );
}

// ─── E043: single-valued header objects name each header once (issue #1062) ──
//
// Since #1050 the engine rejects these documents (400 from POST /imposters, a startup error from
// --configfile). Before this rule, lint passed and deployment failed — the worst ordering for a
// pre-flight tool.

#[test]
fn e043_inject_headers_case_variant_duplicate() {
    let proxy = json!({ "to": "http://x", "injectHeaders": { "X-Id": "a", "x-id": "b" } });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "E043"), "got {:?}", codes(&r));

    let issue = r.issues.iter().find(|i| i.code == "E043").expect("E043");
    assert!(
        issue.message.contains("X-Id") && issue.message.contains("x-id"),
        "the message must name both spellings so the author can find them: {}",
        issue.message
    );
    assert_eq!(
        issue.location.as_deref(),
        Some("loc.injectHeaders"),
        "the location must point at the offending field"
    );
    assert_eq!(issue.severity, Severity::Error, "the engine rejects this");
}

#[test]
fn e043_fault_error_headers_case_variant_duplicate() {
    let response = json!({
        "is": { "statusCode": 200 },
        "_rift": { "fault": { "error": { "headers": { "X-Id": "a", "x-id": "b" } } } }
    });
    let mut r = LintResult::new();
    validate_response(
        path(),
        &response,
        "loc",
        &mut r,
        &opts(),
        &serde_json::Value::Null,
    );
    assert!(has_code(&r, "E043"), "got {:?}", codes(&r));
    let issue = r.issues.iter().find(|i| i.code == "E043").expect("E043");
    assert_eq!(
        issue.location.as_deref(),
        Some("loc._rift.fault.error.headers")
    );
}

#[test]
fn e043_not_fired_for_a_single_spelling() {
    let proxy = json!({ "to": "http://x", "injectHeaders": { "X-Id": "a", "X-Other": "b" } });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(!has_code(&r, "E043"), "got {:?}", codes(&r));
}

#[test]
fn e043_not_fired_for_is_headers() {
    // `is.headers` is multi-valued and the engine *folds* a case-variant pair there (#1039)
    // rather than rejecting it, so flagging it would be a false positive.
    let is_resp = json!({ "headers": { "X-Id": "a", "x-id": "b" } });
    let mut r = LintResult::new();
    validate_is_response(path(), &is_resp, "loc", &mut r);
    assert!(!has_code(&r, "E043"), "got {:?}", codes(&r));
}

#[test]
fn e043_reports_a_non_object_inject_headers_as_e021() {
    let proxy = json!({ "to": "http://x", "injectHeaders": "not-an-object" });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert!(has_code(&r, "E021"), "got {:?}", codes(&r));
}

#[test]
fn e043_fires_once_per_duplicate_not_once_per_key() {
    let proxy = json!({
        "to": "http://x",
        "injectHeaders": { "X-Id": "a", "x-id": "b", "X-ID": "c" }
    });
    let mut r = LintResult::new();
    validate_proxy_response(path(), &proxy, "loc", &mut r);
    assert_eq!(
        r.issues.iter().filter(|i| i.code == "E043").count(),
        2,
        "three spellings of one name are two duplicates, got {:?}",
        codes(&r)
    );
}

// ─── E044 — byte-identical duplicate key (issue #1069) ──────────────────────
//
// The gap the old `e043_does_not_yet_catch_a_byte_identical_duplicate` pinned is closed. That test
// said "if this starts failing, the gap was closed — update it, don't relax it", so it is replaced
// here by the two tests that state the new contract: the text path reports it, and `lint_value`
// still cannot (it is handed an already-collapsed `Value`).

const DUP_INJECT_HEADERS: &str = r#"{"port":3000,"protocol":"http","stubs":[
    {"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}]}
]}"#;

#[test]
fn e044_reports_a_byte_identical_duplicate_from_the_text() {
    let r = lint_json(DUP_INJECT_HEADERS, "<test>", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E044").collect();
    assert_eq!(hits.len(), 1, "expected one E044, got {:?}", codes(&r));
    assert_eq!(
        hits[0].location.as_deref(),
        Some("stubs[0].responses[0].proxy.injectHeaders"),
        "E044 names the object that holds the duplicate"
    );
    assert!(
        hits[0].message.contains("X-Id"),
        "the message names the repeated key, got {:?}",
        hits[0].message
    );
    assert!(
        !has_code(&r, "E043"),
        "a byte-identical duplicate is E044, not E043, got {:?}",
        codes(&r)
    );
}

#[test]
fn lint_value_cannot_see_a_byte_identical_duplicate() {
    // The documented limitation of the `Value`-only entry point: `serde_json::Map` is last-wins, so
    // by the time a caller has a `Value` the second `X-Id` is already gone. Callers that need E044
    // must use `parse_document`/`lint_document` (or `lint_json`/`lint_file`, which do).
    let value: Value =
        serde_json::from_str(DUP_INJECT_HEADERS).expect("parses; duplicate collapsed");
    let r = lint_value(&value, "<test>", &opts());
    assert!(
        !has_code(&r, "E044") && !has_code(&r, "E043"),
        "lint_value sees a collapsed map and must report neither, got {:?}",
        codes(&r)
    );
}

#[test]
fn e044_finds_the_field_through_every_document_shape() {
    // E044 matches a duplicate's path against the single-valued header objects it finds in the
    // parsed value, so the wrapper forms have to agree on the prefix. A mismatch here would make
    // the rule silently report nothing for `{"imposters": [...]}` and bare-array documents.
    let stub =
        r#"{"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}]}"#;
    let single = format!(r#"{{"port":3000,"protocol":"http","stubs":[{stub}]}}"#);
    let wrapped =
        format!(r#"{{"imposters":[{{"port":3000,"protocol":"http","stubs":[{stub}]}}]}}"#);
    let bare = format!(r#"[{{"port":3000,"protocol":"http","stubs":[{stub}]}}]"#);

    // Issue #1094 moved the prefix dispatch into `imposters_in`; pin the exact locations.
    for (raw, location) in [
        (&single, "stubs[0].responses[0].proxy.injectHeaders"),
        (
            &wrapped,
            "imposters[0].stubs[0].responses[0].proxy.injectHeaders",
        ),
        (&bare, "[0].stubs[0].responses[0].proxy.injectHeaders"),
    ] {
        let r = lint_json(raw, "<test>", &opts());
        let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E044").collect();
        assert_eq!(hits.len(), 1, "{location}: got {:?}", codes(&r));
        assert_eq!(hits[0].location.as_deref(), Some(location));
    }
}

#[test]
fn e044_does_not_report_a_repeated_struct_field() {
    // The engine *does* reject `{"port":3000,"port":3001}` on a text path (serde's derive errors
    // with `duplicate field`), but recognising a known struct field needs the schema. Deliberately
    // out of scope: a false negative here, never a false positive.
    let raw = r#"{"port":3000,"port":3001,"protocol":"http","stubs":[]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
}

#[test]
fn e044_reports_one_finding_per_repeated_occurrence() {
    // Three copies of one header are two duplicates — E043's three-spelling convention.
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"proxy":{"to":"http://x",
          "injectHeaders":{"X-Id":"a","X-Id":"b","X-Id":"c"}}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert_eq!(
        r.issues.iter().filter(|i| i.code == "E044").count(),
        2,
        "got {:?}",
        codes(&r)
    );
}

#[test]
fn e044_is_scoped_to_a_single_object() {
    // The same key in two sibling objects is not a duplicate.
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200}}]},
        {"responses":[{"is":{"statusCode":201}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
}

#[test]
fn e044_reports_a_duplicate_under_fault_error_headers() {
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200},
          "_rift":{"fault":{"error":{"headers":{"X-Id":"a","X-Id":"b"}}}}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E044").collect();
    assert_eq!(hits.len(), 1, "got {:?}", codes(&r));
    assert_eq!(
        hits[0].location.as_deref(),
        Some("stubs[0].responses[0]._rift.fault.error.headers")
    );
}

#[test]
fn e044_does_not_fire_on_is_headers_where_a_repeat_is_deliberate() {
    // `is.headers` is MULTI-valued: a repeated key is merged into two header lines, which is how a
    // stub sends two `Set-Cookie`s. `rift_types::wire`'s
    // `headers_merge_byte_identical_duplicate_keys` pins that the engine ACCEPTS it. Flagging this
    // would report a working, documented idiom as an error.
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,
          "headers":{"Set-Cookie":"a=1","Set-Cookie":"b=2"}}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
}

#[test]
fn e044_does_not_fire_inside_a_free_form_body() {
    // `is.body` is an arbitrary `serde_json::Value`; last-wins applies and the engine accepts the
    // document, so a repeat there is not a deploy failure waiting to happen.
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"body":{"a":1,"a":2}}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
}

#[test]
fn e044_does_not_fire_on_a_case_variant_pair() {
    // That shape survives into the `Value` and is E043's job; reporting both would double-count.
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","x-id":"b"}}}]}
    ]}"#;
    let r = lint_json(raw, "<test>", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
    assert!(
        has_code(&r, "E043"),
        "E043 still owns the case-variant shape, got {:?}",
        codes(&r)
    );
}

#[test]
fn malformed_json_is_still_e001_not_a_panic() {
    let r = lint_json("{not json}", "<test>", &opts());
    assert!(has_code(&r, "E001"), "got {:?}", codes(&r));
    assert!(!has_code(&r, "E044"));
}

#[test]
fn documents_with_nothing_repeated_report_no_e044() {
    for raw in [
        r#"{}"#,
        r#"[]"#,
        r#"{"port":3000,"protocol":"http","stubs":[]}"#,
    ] {
        let r = lint_json(raw, "<test>", &opts());
        assert!(!has_code(&r, "E044"), "{raw} produced {:?}", codes(&r));
    }
}

// ─── E045 — non-string value in a single-valued header object (issue #1069) ──

fn inject_headers(headers: Value) -> Value {
    make_imposter(json!([{
        "responses": [{ "proxy": { "to": "http://x", "injectHeaders": headers } }]
    }]))
}

#[test]
fn e045_flags_every_non_string_kind_and_names_it() {
    // Engine-side these fields are `HashMap<String, String>`, so each of these is a hard 400.
    for (value, kind) in [
        (json!(1), "number"),
        (json!(true), "boolean"),
        (json!(null), "null"),
        (json!(["a", "b"]), "array"),
        (json!({"a": "b"}), "object"),
    ] {
        let r = lint_value(&inject_headers(json!({ "X-Id": value })), "<test>", &opts());
        let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E045").collect();
        assert_eq!(hits.len(), 1, "{kind}: got {:?}", codes(&r));
        assert!(
            hits[0].message.contains(kind) && hits[0].message.contains("X-Id"),
            "{kind}: message must name the header and the kind, got {:?}",
            hits[0].message
        );
    }
}

#[test]
fn e045_suggests_the_right_fix_for_each_kind() {
    // Three branches of guidance text that no other test reads; a swap between them would
    // otherwise ship silently.
    let cases = [
        (json!(1), "Change to: \"X-Id\": \"1\""),
        (json!(true), "Change to: \"X-Id\": \"true\""),
        (
            json!(["a"]),
            "Give one string; a header with several values belongs in is.headers",
        ),
        (json!(null), "Remove the header or give it a string value"),
        (
            json!({"a": "b"}),
            "Remove the header or give it a string value",
        ),
    ];
    for (value, expected) in cases {
        let r = lint_value(&inject_headers(json!({ "X-Id": value })), "<test>", &opts());
        let hit = r
            .issues
            .iter()
            .find(|i| i.code == "E045")
            .expect("E045 present");
        assert_eq!(hit.suggestion.as_deref(), Some(expected));
    }
}

#[test]
fn e045_does_not_fire_on_an_all_string_map() {
    let r = lint_value(
        &inject_headers(json!({ "X-Id": "a", "X-Other": "b" })),
        "<test>",
        &opts(),
    );
    assert!(!has_code(&r, "E045"), "got {:?}", codes(&r));
}

#[test]
fn e045_covers_fault_error_headers_too() {
    let imposter = make_imposter(json!([{
        "responses": [{
            "is": { "statusCode": 200 },
            "_rift": { "fault": { "error": { "headers": { "X-Id": 1 } } } }
        }]
    }]));
    let r = lint_value(&imposter, "<test>", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E045").collect();
    assert_eq!(hits.len(), 1, "got {:?}", codes(&r));
    assert_eq!(
        hits[0].location.as_deref(),
        Some("stubs[0].responses[0]._rift.fault.error.headers")
    );
}

#[test]
fn e045_does_not_touch_is_headers() {
    // `is.headers` is multi-valued: a number there is E019 and a string array is legal (#238).
    // Reusing E045 for it would contradict both.
    let numeric = make_imposter(json!([{
        "responses": [{ "is": { "statusCode": 200, "headers": { "Content-Length": 256 } } }]
    }]));
    let r = lint_value(&numeric, "<test>", &opts());
    assert!(has_code(&r, "E019"), "got {:?}", codes(&r));
    assert!(!has_code(&r, "E045"), "got {:?}", codes(&r));

    let array = make_imposter(json!([{
        "responses": [{ "is": { "statusCode": 200, "headers": { "Accept": ["a", "b"] } } }]
    }]));
    let r = lint_value(&array, "<test>", &opts());
    assert!(!has_code(&r, "E045"), "got {:?}", codes(&r));
    assert!(
        !has_code(&r, "E018"),
        "a string array is legal in is.headers, got {:?}",
        codes(&r)
    );
}

#[test]
fn e045_and_e043_can_both_fire_on_one_map() {
    let r = lint_value(
        &inject_headers(json!({ "X-Id": "a", "x-id": 1 })),
        "<test>",
        &opts(),
    );
    assert!(has_code(&r, "E043"), "got {:?}", codes(&r));
    assert!(has_code(&r, "E045"), "got {:?}", codes(&r));
}

#[test]
fn a_non_object_single_valued_header_field_is_still_e021() {
    let r = lint_value(&inject_headers(json!("nope")), "<test>", &opts());
    assert!(has_code(&r, "E021"), "got {:?}", codes(&r));
    assert!(!has_code(&r, "E045"), "got {:?}", codes(&r));
}

#[test]
fn e043_surfaces_through_a_whole_imposter_document() {
    // The unit hooks above prove the helper is called; this proves the wiring survives the real
    // entry point a user actually runs `rift-lint` through.
    let imposter = make_imposter(json!([{
        "responses": [{ "proxy": { "to": "http://x", "injectHeaders": { "X-Id": "a", "x-id": "b" } } }]
    }]));
    let r = lint_value(&imposter, "<test>", &opts());
    assert!(has_code(&r, "E043"), "got {:?}", codes(&r));
}

/// Issue #1076: the accessor `--fix` consults is deliberately **document-wide**, not E044's two
/// fields. Anything that rewrites a file from the collapsed value loses every repeated key, not
/// only the ones the engine rejects — and the `is.headers` one, which E044 pointedly does not
/// report, is the one whose loss is otherwise completely silent.
#[test]
fn duplicate_keys_lists_every_repeat_document_wide() {
    let raw = r#"{"port":3000,"port":3001,"protocol":"http","stubs":[
        {"responses":[
            {"is":{"statusCode":200,"headers":{"Set-Cookie":"a=1","Set-Cookie":"b=2"}}},
            {"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}
        ]}
    ]}"#;
    let doc = rift_lint::parse_document(raw).expect("parses");
    let found: Vec<(Option<&str>, &str)> = doc.duplicate_keys().collect();

    assert!(
        found.contains(&(None, "port")),
        "the root repeat is listed, got {found:?}"
    );
    assert!(
        found.contains(&(Some("stubs[0].responses[0].is.headers"), "Set-Cookie")),
        "the deliberate is.headers repeat is listed even though E044 ignores it, got {found:?}"
    );
    assert!(
        found.contains(&(Some("stubs[0].responses[1].proxy.injectHeaders"), "X-Id")),
        "the E044 repeat is listed, got {found:?}"
    );
    assert_eq!(found.len(), 3, "exactly those three, got {found:?}");
}

/// A clean document yields nothing, so `--fix` is never refused for a file with no duplicates.
#[test]
fn duplicate_keys_is_empty_for_a_clean_document() {
    let doc = rift_lint::parse_document(
        r#"{"port":3000,"protocol":"http","stubs":[{"responses":[{"is":{"statusCode":200}}]}]}"#,
    )
    .expect("parses");
    assert_eq!(doc.duplicate_keys().count(), 0);
}

// ─── Issue #1071: YAML documents are linted, and the one shape the engine rejects is reported ──

const YAML_SEQUENCE: &str = "\
- port: 3000
  protocol: http
  stubs:
    - responses:
        - is:
            statusCode: 200
            headers:
              Content-Length: 256
";

/// A YAML imposter sequence is linted like any other document: ordinary rules fire. Before this,
/// `rift-lint config.yaml` exited 0 having checked nothing.
#[test]
fn a_yaml_sequence_is_linted_like_json() {
    let r = rift_lint::lint_yaml(YAML_SEQUENCE, "<test>.yaml", &opts());
    assert!(
        has_code(&r, "E019"),
        "the numeric header is reported in YAML too, got {:?}",
        codes(&r)
    );
}

/// E046: the engine's YAML path is `from_str::<Vec<ImposterConfig>>`, so only a top-level sequence
/// loads. The linter accepts three document shapes in JSON, and without this rule two of them would
/// lint clean and then fail at startup — the exact ordering this crate exists to prevent.
#[test]
fn e046_reports_a_yaml_mapping_root() {
    let single = "\
port: 3000
protocol: http
stubs: []
";
    let r = rift_lint::lint_yaml(single, "<test>.yaml", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E046").collect();
    assert_eq!(hits.len(), 1, "a bare mapping root, got {:?}", codes(&r));
    // The message is the rule's whole payload: it has to say what the engine requires and what to
    // write instead, or an author learns only that something is wrong.
    assert!(
        hits[0].message.contains("sequence of imposters"),
        "names what the engine requires, got {:?}",
        hits[0].message
    );
    assert!(
        hits[0]
            .suggestion
            .as_deref()
            .is_some_and(|s| s.contains("- port")),
        "shows the shape to write, got {:?}",
        hits[0].suggestion
    );
}

/// The `{"imposters": [...]}` wrapper is legal JSON for `--configfile` but unreachable from YAML.
#[test]
fn e046_reports_a_yaml_imposters_wrapper() {
    let wrapper = "\
imposters:
  - port: 3000
    protocol: http
    stubs: []
";
    let r = rift_lint::lint_yaml(wrapper, "<test>.yaml", &opts());
    assert!(
        has_code(&r, "E046"),
        "the wrapper form, got {:?}",
        codes(&r)
    );
}

#[test]
fn e046_does_not_fire_on_a_sequence_root() {
    let r = rift_lint::lint_yaml(YAML_SEQUENCE, "<test>.yaml", &opts());
    assert!(!has_code(&r, "E046"), "got {:?}", codes(&r));
}

/// E046 is about YAML only: all three shapes remain valid for a JSON `--configfile`.
#[test]
fn e046_never_fires_on_json() {
    for raw in [
        r#"{"port":3000,"protocol":"http","stubs":[]}"#,
        r#"{"imposters":[{"port":3000,"protocol":"http","stubs":[]}]}"#,
        r#"[{"port":3000,"protocol":"http","stubs":[]}]"#,
    ] {
        let r = lint_json(raw, "<test>", &opts());
        assert!(!has_code(&r, "E046"), "{raw} produced {:?}", codes(&r));
    }
}

/// A YAML parse failure must not be reported as invalid JSON.
#[test]
fn malformed_yaml_is_e001_naming_yaml() {
    let r = rift_lint::lint_yaml("port: [unclosed", "<test>.yaml", &opts());
    let e001: Vec<_> = r.issues.iter().filter(|i| i.code == "E001").collect();
    assert_eq!(e001.len(), 1, "got {:?}", codes(&r));
    assert!(
        e001[0].message.contains("YAML"),
        "the message names the format, got {:?}",
        e001[0].message
    );
}

/// `serde_yaml` refuses a multi-document stream, which is the same answer `--configfile` gives.
#[test]
fn a_multi_document_yaml_stream_is_e001() {
    let two = "\
- port: 3000
  protocol: http
  stubs: []
---
- port: 3001
  protocol: http
  stubs: []
";
    let r = rift_lint::lint_yaml(two, "<test>.yaml", &opts());
    assert!(has_code(&r, "E001"), "got {:?}", codes(&r));
}

/// E044 works on YAML too: the engine's YAML path is a text path, so a repeated single-valued
/// header name is a startup error there exactly as it is for JSON.
#[test]
fn e044_is_reported_from_yaml_text() {
    let dup = "\
- port: 3000
  protocol: http
  stubs:
    - responses:
        - proxy:
            to: http://x
            injectHeaders:
              X-Id: a
              X-Id: b
";
    let r = rift_lint::lint_yaml(dup, "<test>.yaml", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "E044").collect();
    assert_eq!(hits.len(), 1, "got {:?}", codes(&r));
    assert_eq!(
        hits[0].location.as_deref(),
        Some("[0].stubs[0].responses[0].proxy.injectHeaders")
    );
}

/// An alias expands to its anchor before the scan, so reusing a clean mapping is not a duplicate.
#[test]
fn a_yaml_alias_is_not_a_duplicate_key() {
    let aliased = "\
- port: 3000
  protocol: http
  stubs:
    - responses:
        - is: &ok
            statusCode: 200
    - responses:
        - is: *ok
";
    let r = rift_lint::lint_yaml(aliased, "<test>.yaml", &opts());
    assert!(!has_code(&r, "E044"), "got {:?}", codes(&r));
}

/// `lint_directory` walks YAML as well as JSON, and still ignores everything else.
#[test]
fn lint_directory_reads_yaml_and_yml_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("a.json"),
        r#"{"port":3000,"protocol":"http","stubs":[]}"#,
    )
    .expect("write");
    std::fs::write(dir.path().join("b.yaml"), YAML_SEQUENCE).expect("write");
    std::fs::write(dir.path().join("c.yml"), YAML_SEQUENCE).expect("write");
    std::fs::write(dir.path().join("readme.txt"), "hello").expect("write");

    let result = lint_directory(dir.path(), &opts());
    assert_eq!(
        result.files_checked, 3,
        "the two YAML files and the JSON one, not the .txt"
    );
    // Counting is not linting: a `.yml` routed through the JSON parser would fail with E001 and
    // still be counted. The E019s are what prove both YAML files were read as YAML.
    assert_eq!(
        result.issues.iter().filter(|i| i.code == "E019").count(),
        2,
        "one from the .yaml and one from the .yml, got {:?}",
        codes(&result)
    );
    assert!(
        !result.issues.iter().any(|i| i.code == "E001"),
        "nothing was mis-parsed, got {:?}",
        codes(&result)
    );
}

/// A bad root shape must not mask the rest: E046 is appended alongside every other finding, not
/// instead of them. Nothing short-circuits today, and this is what keeps it that way.
#[test]
fn e046_does_not_suppress_the_other_findings() {
    let mapping_root_missing_protocol = "\
port: 3000
stubs: []
";
    let r = rift_lint::lint_yaml(mapping_root_missing_protocol, "<test>.yaml", &opts());
    assert!(
        has_code(&r, "E046"),
        "the root shape is reported, got {:?}",
        codes(&r)
    );
    assert!(
        r.issues.iter().any(|i| i.code != "E046"),
        "and so is the rest of the document, got {:?}",
        codes(&r)
    );
}

/// `lint_value` has neither raw text nor a format, so it cannot report E046 any more than it can
/// report E044.
#[test]
fn lint_value_cannot_report_e046() {
    let mapping_root = json!({ "port": 3000, "protocol": "http", "stubs": [] });
    let r = lint_value(&mapping_root, "<test>", &opts());
    assert!(!has_code(&r, "E046"), "got {:?}", codes(&r));
}

/// The engine keys off content, not the extension, so a `.yaml` file holding JSON is loaded by its
/// JSON branch and E046 must stay quiet. Keying E046 off the extension would flag a working config.
#[test]
fn e046_does_not_fire_for_yaml_text_that_is_actually_json() {
    for text in [
        r#"{"imposters":[{"port":3000,"protocol":"http","stubs":[]}]}"#,
        r#"{"port":3000,"protocol":"http","stubs":[]}"#,
        r#"  [{"port":3000,"protocol":"http","stubs":[]}]"#,
    ] {
        let r = rift_lint::lint_yaml(text, "<test>.yaml", &opts());
        assert!(
            !has_code(&r, "E046"),
            "{text} is read by the engine's JSON branch, got {:?}",
            codes(&r)
        );
    }
}

// ─── Issue #1083: W012, a number the engine cannot serve as written ─────────────────────────────

const LOSSY_BODY: &str = r#"{"port":3000,"protocol":"http","stubs":[
    {"responses":[{"is":{"statusCode":200,
        "body":{"big":123456789012345678901234567890,"precise":0.1000000000000000055511151231257827}}}]}
]}"#;

/// The engine holds a JSON number as the nearest `u64`/`i64`/`f64`, so these two are served as
/// `1.2345678901234568e29` and `0.1` — and nothing said so.
#[test]
fn w012_reports_each_number_the_engine_cannot_serve_as_written() {
    let r = rift_lint::lint_json(LOSSY_BODY, "<test>.json", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "W012").collect();
    assert_eq!(hits.len(), 2, "got {:?}", codes(&r));
    assert!(hits.iter().all(|i| i.severity == Severity::Warning));

    assert!(
        hits[0].message.contains("'123456789012345678901234567890'")
            && hits[0].message.contains("1.2345678901234568e29"),
        "names the literal and what is served, got: {}",
        hits[0].message
    );
    assert_eq!(hits[0].location.as_deref(), Some("line 3, column 23"));
    assert!(
        hits[1]
            .message
            .contains("'0.1000000000000000055511151231257827'")
            && hits[1].message.ends_with("nearest double, 0.1"),
        "got: {}",
        hits[1].message
    );
    assert!(
        hits[0]
            .suggestion
            .as_deref()
            .is_some_and(|s| s.contains("string")),
        "says how to keep the digits, got {:?}",
        hits[0].suggestion
    );
    assert!(
        !r.has_errors(),
        "a warning, never an error: {:?}",
        codes(&r)
    );
}

/// Formatting is not loss, and neither is an ordinary float a double holds exactly — which relies
/// on `float_roundtrip` (#1085); without it `7e23` and `0.10018513143495411` would be reported.
#[test]
fn w012_is_silent_for_numbers_served_with_the_same_value() {
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,
            "body":{"a":0.10,"b":1e2,"c":18446744073709551615,"d":7e23,"e":0.10018513143495411,"f":-0}}}]}
    ]}"#;
    let r = rift_lint::lint_json(raw, "<test>.json", &opts());
    assert!(!has_code(&r, "W012"), "got {:?}", codes(&r));
}

#[test]
fn w012_ignores_digits_inside_a_string() {
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"body":"{\"big\": 123456789012345678901234567890}"}}]}
    ]}"#;
    let r = rift_lint::lint_json(raw, "<test>.json", &opts());
    assert!(!has_code(&r, "W012"), "got {:?}", codes(&r));
}

/// Documented gap: the scan is a JSON lexer, so a document read as YAML is not checked.
#[test]
fn w012_does_not_check_a_yaml_document() {
    let yaml = "\
- port: 3000
  protocol: http
  stubs:
    - responses:
        - is:
            statusCode: 200
            body:
              big: 123456789012345678901234567890
";
    let r = rift_lint::lint_yaml(yaml, "<test>.yaml", &opts());
    assert!(!has_code(&r, "W012"), "got {:?}", codes(&r));
}

/// Not only bodies: a predicate compares against the rounded value too, so the message says what the
/// engine *reads*, not what it serves.
#[test]
fn w012_reports_a_number_outside_a_response_body_in_neutral_terms() {
    let raw = r#"{"port":3000,"protocol":"http","stubs":[
        {"predicates":[{"equals":{"body":{"id":123456789012345678901234567890}}}],
         "responses":[{"is":{"statusCode":200}}]}
    ]}"#;
    let r = rift_lint::lint_json(raw, "<test>.json", &opts());
    let hits: Vec<_> = r.issues.iter().filter(|i| i.code == "W012").collect();
    assert_eq!(hits.len(), 1, "got {:?}", codes(&r));
    assert!(
        hits[0]
            .message
            .contains("the engine reads it as the nearest double, 1.2345678901234568e29")
            && !hits[0].message.contains("served"),
        "got: {}",
        hits[0].message
    );
}

/// Issue #1094: every imposter a document holds, with the location prefix of its slot, for the
/// three shapes `--configfile` loads.
#[test]
fn imposters_in_yields_each_shape_with_its_prefix() {
    let single = json!({"port": 1, "protocol": "http"});
    let slots: Vec<(String, Value)> = imposters_in(&single)
        .into_iter()
        .map(|(p, v)| (p, v.clone()))
        .collect();
    assert_eq!(
        slots,
        vec![(String::new(), json!({"port": 1, "protocol": "http"}))]
    );

    let wrapper = json!({"imposters": [{"port": 1}, {"port": 2}]});
    let slots: Vec<(String, Value)> = imposters_in(&wrapper)
        .into_iter()
        .map(|(p, v)| (p, v.clone()))
        .collect();
    assert_eq!(
        slots,
        vec![
            ("imposters[0].".to_string(), json!({"port": 1})),
            ("imposters[1].".to_string(), json!({"port": 2})),
        ]
    );

    let array = json!([{"port": 1}, {"port": 2}]);
    let slots: Vec<(String, Value)> = imposters_in(&array)
        .into_iter()
        .map(|(p, v)| (p, v.clone()))
        .collect();
    assert_eq!(
        slots,
        vec![
            ("[0].".to_string(), json!({"port": 1})),
            ("[1].".to_string(), json!({"port": 2})),
        ]
    );

    assert!(imposters_in(&json!({"imposters": []})).is_empty());
    assert!(imposters_in(&json!([])).is_empty());
}

// ─── W015: a binary-mode body the engine cannot decode (issue #1151) ────────────────────────
//
// A warning, not an error: the engine *serves* these — it falls back to the raw body with
// `x-rift-binary-error: true`, or answers 500 under `strictBehaviors` — so the file is valid, it
// just will not do what its author meant. The decode must be the engine's exact one
// (`base64::engine::general_purpose::STANDARD`), not a regex approximation.

#[test]
fn w015_fires_for_an_undecodable_binary_is_body() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{ "is": {
            "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary"
        } }] }]
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(has_code(&r, "W015"), "got {:?}", codes(&r));
}

// The linter did not inspect `defaultResponse` at all before this — its first rule there.
#[test]
fn w015_fires_for_an_undecodable_binary_default_response() {
    let imposter = json!({
        "port": 4545, "protocol": "http", "stubs": [],
        "defaultResponse": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" }
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(has_code(&r, "W015"), "got {:?}", codes(&r));
}

// A non-string body is serialized to JSON text before the decode, so in binary mode it can never
// decode — and is served as that JSON text.
#[test]
fn w015_fires_for_a_non_string_binary_body() {
    let imposter = json!({
        "port": 4545, "protocol": "http", "stubs": [],
        "defaultResponse": { "statusCode": 200, "body": { "a": 1 }, "_mode": "binary" }
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(has_code(&r, "W015"), "got {:?}", codes(&r));
}

#[test]
fn w015_is_silent_for_valid_base64() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "aGVsbG8=", "_mode": "binary" } }] }],
        "defaultResponse": { "statusCode": 200, "body": "aGVsbG8=", "_mode": "binary" }
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(!has_code(&r, "W015"), "got {:?}", codes(&r));
}

// Text mode never decodes, so an arbitrary body there is not this rule's business.
#[test]
fn w015_is_silent_in_text_mode() {
    let imposter = json!({
        "port": 4545, "protocol": "http", "stubs": [],
        "defaultResponse": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "text" }
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(!has_code(&r, "W015"), "got {:?}", codes(&r));
}

// The flat response form (issue #304) — no `is` wrapper — is decoded by the engine identically, so
// it must be linted identically. The first cut only checked inside `is` and missed it.
#[test]
fn w015_fires_for_an_undecodable_flat_form_response() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{
            "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary"
        }] }]
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(has_code(&r, "W015"), "got {:?}", codes(&r));
}

// A body the engine rewrites before decoding is not what gets decoded, so a non-base64 placeholder
// there can be correct. A warning that fires on a correct file is worse than silence.
#[test]
fn w015_stands_down_when_templating_rewrites_the_body() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{
            "is": { "statusCode": 200, "body": "{{ placeholder }}", "_mode": "binary" },
            "_rift": { "templated": true }
        }] }]
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(!has_code(&r, "W015"), "got {:?}", codes(&r));
}

#[test]
fn w015_stands_down_when_a_behavior_rewrites_the_body() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{
            "is": { "statusCode": 200, "body": "${placeholder}", "_mode": "binary" },
            "_behaviors": { "copy": { "from": "path", "into": "${placeholder}", "using": { "method": "regex", "selector": ".*" } } }
        }] }]
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(!has_code(&r, "W015"), "got {:?}", codes(&r));
}

// A behavior that does NOT touch the body (a `wait`) must not switch the rule off.
#[test]
fn w015_still_fires_alongside_a_behavior_that_does_not_rewrite_the_body() {
    let imposter = json!({
        "port": 4545, "protocol": "http",
        "stubs": [{ "responses": [{
            "is": { "statusCode": 200, "body": "not!valid!base64!", "_mode": "binary" },
            "_behaviors": { "wait": 100 }
        }] }]
    });
    let mut r = LintResult::new();
    validate_imposter(path(), &imposter, &mut r, &opts());
    assert!(has_code(&r, "W015"), "got {:?}", codes(&r));
}

// The engine reads a null or absent body as "", which decodes to empty bytes — valid. Pinned so a
// refactor cannot turn the early return into a false positive.
#[test]
fn w015_is_silent_for_a_null_or_absent_binary_body() {
    for default in [
        json!({ "statusCode": 204, "body": null, "_mode": "binary" }),
        json!({ "statusCode": 204, "_mode": "binary" }),
    ] {
        let imposter = json!({
            "port": 4545, "protocol": "http", "stubs": [], "defaultResponse": default
        });
        let mut r = LintResult::new();
        validate_imposter(path(), &imposter, &mut r, &opts());
        assert!(!has_code(&r, "W015"), "for {default}: got {:?}", codes(&r));
    }
}
