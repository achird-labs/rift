//! Offline synthesis of test requests from stub JSON, shared by `rift-verify` and other
//! callers that need "what request does this stub match, and what does it answer?" without a
//! running server.
//!
//! Everything here is pure over `serde_json::Value` stubs: no network, no output formatting.

use base64::Engine;
use serde::Deserialize;
use std::collections::HashMap;

/// A stub as the generator reads it (the `predicates`/`responses` of one imposter stub).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stub {
    #[serde(default)]
    pub id: Option<String>,
    /// Correlated-isolation partition (issue #223): when set, the stub is eligible only for the
    /// flow id equal to this. The verifier must drive it with this value (issue #260).
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub predicates: Vec<serde_json::Value>,
    #[serde(default)]
    pub responses: Vec<serde_json::Value>,
}

/// One synthesised check for a stub: the request to send and the response its first `is` expects,
/// or a skip (`skip_reason`) when the stub cannot be checked statically.
#[derive(Debug, Clone)]
pub struct TestCase {
    pub stub_index: usize,
    pub stub_id: Option<String>,
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    pub body: Option<String>,
    pub expected_status: u16,
    pub expected_headers: HashMap<String, String>,
    pub expected_body: Option<serde_json::Value>,
    pub is_dynamic: bool,
    pub skip_reason: Option<String>,
    /// Stub is designed to never match (contains "DONT MATCH" or similar in predicates)
    pub is_no_match_stub: bool,
    /// Imposter protocol ("http"/"https") — selects the request scheme (issue #206).
    pub protocol: String,
    /// Stub injects a `_rift.fault.tcp` reset (issue #239): a connection error is the expected
    /// outcome, not a failure (finding #3).
    pub expects_transport_error: bool,
    /// Correlated-isolation header name to send (issue #223).
    pub flow_header: Option<String>,
    /// The stub's own `space` partition, if any (issue #260): sent as the flow header value so a
    /// space-gated stub is eligible. `None` ⇒ use the global `--space`.
    pub flow_space: Option<String>,
}

/// Build the test case(s) for one stub: a skip (no-match, unsynthesizable xpath, unresolved flow header, dynamic when `skip_dynamic`) or a checkable case built from the first response. `stub_index` is recorded on each case.
pub fn generate_test_cases(
    stub_index: usize,
    stub: &Stub,
    skip_dynamic: bool,
    protocol: &str,
    flow_header: Option<&str>,
) -> Vec<TestCase> {
    let mut test_cases = Vec::new();

    // Check if this stub has dynamic responses
    let (is_dynamic, dynamic_type) = check_if_dynamic(&stub.responses);

    // A `_rift.fault.tcp` stub resets the connection (issue #239): a transport error is the
    // expected outcome rather than a failure (finding #3).
    let expects_transport_error = expects_tcp_fault(&stub.responses);

    // Check if this stub is designed to never match
    let is_no_match_stub = check_if_no_match_stub(&stub.predicates);

    // Parse predicates to build test request (needed for all cases)
    let (method, path, headers, query_params, body) = parse_predicates(&stub.predicates);
    let flow_header = flow_header.map(str::to_string);
    let flow_space = stub.space.clone();

    // No-match stubs (e.g., "DONT MATCH THIS") are designed to never match any request.
    // We mark them as passed because:
    // 1. Testing them would hit other broader stubs that DO match the path
    // 2. Their purpose is to ensure they don't accidentally match real traffic
    // 3. Their existence in the config is the test - they pass by design
    if is_no_match_stub {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic: false,
            skip_reason: Some("no-match stub (passes by design)".to_string()),
            is_no_match_stub: true,
            protocol: protocol.to_string(),
            expects_transport_error: false,
            flow_header: flow_header.clone(),
            flow_space: flow_space.clone(),
        });
        return test_cases;
    }

    // An xpath predicate whose selector can't be synthesized into a matching XML body would always
    // fail to match — surface it as a visible skip rather than a false failure (issue #261).
    if let Some(selector) = unsynthesizable_xpath(&stub.predicates) {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic,
            skip_reason: Some(format!(
                "xpath selector `{selector}` is too complex to synthesize a matching body"
            )),
            is_no_match_stub: false,
            protocol: protocol.to_string(),
            expects_transport_error,
            flow_header: flow_header.clone(),
            flow_space: flow_space.clone(),
        });
        return test_cases;
    }

    // Correlated isolation declared (stub has a `space`) but the verifier couldn't resolve the
    // flowIdSource header — driving it would silently mis-route. Surface it as a visible skip
    // rather than running a degraded check (issue #260).
    if flow_space.is_some() && flow_header.is_none() {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic,
            skip_reason: Some(
                "stub declares `space` but flowIdSource is not discoverable — pass --flow-id-header"
                    .to_string(),
            ),
            is_no_match_stub: false,
            protocol: protocol.to_string(),
            expects_transport_error,
            flow_header: flow_header.clone(),
            flow_space,
        });
        return test_cases;
    }

    // If skipping dynamic and this is dynamic, mark as skipped
    if is_dynamic && skip_dynamic {
        test_cases.push(TestCase {
            stub_index,
            stub_id: stub.id.clone(),
            method,
            path,
            headers,
            query_params,
            body,
            expected_status: 200,
            expected_headers: HashMap::new(),
            expected_body: None,
            is_dynamic: true,
            skip_reason: dynamic_type,
            is_no_match_stub: false,
            protocol: protocol.to_string(),
            expects_transport_error,
            flow_header: flow_header.clone(),
            flow_space: flow_space.clone(),
        });
        return test_cases;
    }

    // Extract expected response from first response
    let (expected_status, expected_headers, expected_body) =
        extract_expected_response(&stub.responses);

    test_cases.push(TestCase {
        stub_index,
        stub_id: stub.id.clone(),
        method,
        path,
        headers,
        query_params,
        body,
        expected_status,
        expected_headers,
        expected_body,
        is_dynamic,
        skip_reason: None,
        is_no_match_stub: false,
        protocol: protocol.to_string(),
        expects_transport_error,
        flow_header,
        flow_space,
    });

    test_cases
}

/// A fault sub-config is certain to fire when `probability` is absent or `>= 1.0`. Mirrors
/// `rift-verify`'s `dynamic::is_certain` (kept private to the bin).
pub fn is_certain(fault_part: &serde_json::Value) -> bool {
    match fault_part.get("probability") {
        None => true,
        Some(p) => p.as_f64().map(|p| p >= 1.0).unwrap_or(false),
    }
}

/// Test cases for every stub of one imposter, in document order, with dynamic stubs reported as
/// skips (`skip_dynamic = true`). `stubs` is the imposter's `stubs` array as saved by
/// `GET /imposters/:port`; each case carries its `stub_index`. A stub that does not deserialize
/// as a [`Stub`] is an error naming its index.
pub fn test_cases_for_stubs(
    stubs: &[serde_json::Value],
    protocol: &str,
    flow_header: Option<&str>,
) -> Result<Vec<TestCase>, String> {
    let mut cases = Vec::new();
    for (index, value) in stubs.iter().enumerate() {
        let stub: Stub = serde_json::from_value(value.clone())
            .map_err(|e| format!("stub {index} is not a valid stub: {e}"))?;
        cases.extend(generate_test_cases(
            index,
            &stub,
            true,
            protocol,
            flow_header,
        ));
    }
    Ok(cases)
}

/// Check if a stub's predicates contain patterns indicating it should never match.
/// These stubs typically have paths like "DONT MATCH THIS" or "DO NOT MATCH THIS"
/// to ensure they never match actual requests.
pub fn check_if_no_match_stub(predicates: &[serde_json::Value]) -> bool {
    let no_match_patterns = [
        "DONT MATCH",
        "DO NOT MATCH",
        "NEVER MATCH",
        "NO MATCH",
        "NOMATCH",
    ];

    for predicate in predicates {
        // Check in equals, contains, startsWith, endsWith predicates
        for key in ["equals", "contains", "startsWith", "endsWith", "deepEquals"] {
            if let Some(pred) = predicate.get(key) {
                // Check path field
                if let Some(path) = pred.get("path").and_then(|v| v.as_str()) {
                    let path_upper = path.to_uppercase();
                    for pattern in &no_match_patterns {
                        if path_upper.contains(pattern) {
                            return true;
                        }
                    }
                }
                // Check body field
                if let Some(body) = pred.get("body").and_then(|v| v.as_str()) {
                    let body_upper = body.to_uppercase();
                    for pattern in &no_match_patterns {
                        if body_upper.contains(pattern) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Whether the stub's responses are dynamic (cycling, inject, proxy, fault, script, stateful behaviors) and so cannot be checked statically, with the reason.
pub fn check_if_dynamic(responses: &[serde_json::Value]) -> (bool, Option<String>) {
    if responses.is_empty() {
        return (false, None);
    }

    // Multiple responses = cycling behavior (stateful, can't predict which response)
    if responses.len() > 1 {
        return (
            true,
            Some(format!("cycling responses ({} responses)", responses.len())),
        );
    }

    let first = &responses[0];

    if first.get("inject").is_some() {
        return (true, Some("inject response (JavaScript)".to_string()));
    }

    // Only treat as proxy if it's a real proxy config (object with "to" field)
    // Many stubs have "proxy": null which should not be treated as dynamic
    if let Some(proxy) = first.get("proxy")
        && proxy.is_object()
        && proxy.get("to").is_some()
    {
        return (true, Some("proxy response".to_string()));
    }

    if first.get("fault").is_some() {
        return (true, Some("fault injection".to_string()));
    }

    // Check for _rift script extension and _rift.fault (tcp/latency/error are non-deterministic
    // or transport-level; can't be asserted as a normal HTTP response).
    if let Some(rift) = first.get("_rift") {
        if rift.get("script").is_some() {
            return (true, Some("Rift script response".to_string()));
        }
        if rift.get("fault").is_some() {
            return (true, Some("Rift fault (_rift.fault)".to_string()));
        }
    }

    // Behaviors whose output depends on the request or external state can't be predicted from
    // the stub alone (repeat=stateful; decorate/copy/lookup/shellTransform=dynamic body/headers).
    // A rift engine's GET /imposters emits the block it already selected and folded (issue #1103):
    // `behaviors` as an array of single-key objects, never `_behaviors`, with `repeat` lifted to the
    // response (#1191), so scanning elements is the engine's own view. The `_behaviors` object form is still read for other admin APIs.
    let label = |k: &str| match k {
        "repeat" => "repeat behavior (stateful)",
        "decorate" => "decorate behavior (dynamic)",
        "copy" => "copy behavior (request-derived)",
        "lookup" => "lookup behavior (data-source)",
        "shellTransform" => "shellTransform behavior (external)",
        _ => "dynamic behavior",
    };
    const DYNAMIC_BEHAVIORS: [&str; 5] = ["repeat", "decorate", "copy", "lookup", "shellTransform"];
    // A key configures a behavior only the way the engine reads it: `null` is the key absent
    // (issue #1093), and `copy`/`lookup`/`shellTransform` are lists, so an empty one runs nothing
    // (issue #1103). An empty array is inert for the other two as well: `repeat` is read with
    // `as_u64`, and an array `decorate` makes the whole block unparseable, so none of it runs.
    let has = |obj: &serde_json::Map<String, serde_json::Value>, k: &str| {
        obj.get(k).is_some_and(|v| match v {
            serde_json::Value::Null => false,
            serde_json::Value::Array(items) => !items.is_empty(),
            _ => true,
        })
    };
    // A rift engine writes `repeat` on the response, as Mountebank does (issue #1191); the block
    // forms below are read for older engines and other admin APIs.
    if first.get("repeat").is_some_and(|r| !r.is_null()) {
        return (true, Some(label("repeat").to_string()));
    }
    // `_behaviors` runs only beside no `behaviors` key and no top-level `repeat` (issue #1709); a
    // Mountebank echo keeps an ignored one, which configures nothing.
    if first.get("behaviors").is_none()
        && let Some(obj) = first.get("_behaviors").and_then(|v| v.as_object())
    {
        for k in DYNAMIC_BEHAVIORS {
            if has(obj, k) {
                return (true, Some(label(k).to_string()));
            }
        }
    }
    if let Some(arr) = first.get("behaviors").and_then(|v| v.as_array()) {
        for item in arr.iter().filter_map(|v| v.as_object()) {
            for k in DYNAMIC_BEHAVIORS {
                if has(item, k) {
                    return (true, Some(label(k).to_string()));
                }
            }
        }
    }

    (false, None)
}

/// True when the stub's first response injects a connection-level TCP fault
/// (`_rift.fault.tcp`, issue #239) that is *certain* to fire, for which a transport error is the
/// expected outcome. The probabilistic object form (`{ probability < 1.0, type }`, issue #531) is
/// non-deterministic — its reset can't be asserted — so it does not expect a transport error.
/// A bare-string `tcp` is certain (`is_certain` treats an absent `probability` as always-fires).
pub fn expects_tcp_fault(responses: &[serde_json::Value]) -> bool {
    responses
        .first()
        .and_then(|r| r.get("_rift"))
        .and_then(|r| r.get("fault"))
        .and_then(|f| f.get("tcp"))
        .is_some_and(is_certain)
}

/// Decode a base64 `_mode:binary` body to the served UTF-8 string (issue #273). A non-string body,
/// invalid base64, or non-UTF-8 bytes are left as-is — the engine itself falls back to the raw body.
/// A genuinely non-UTF-8 binary body therefore reports a body mismatch (it can't be string-compared),
/// which is the safe direction for a verifier — a loud failure, never a false pass.
pub fn decode_binary_body(body: Option<serde_json::Value>) -> Option<serde_json::Value> {
    match body {
        Some(serde_json::Value::String(b64)) => base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(serde_json::Value::String)
            .or(Some(serde_json::Value::String(b64))),
        other => other,
    }
}

/// Status, headers and body the stub's first response serves (`is` form, `_mode: binary` decoded; proxy/inject/fault get loose expectations).
pub fn extract_expected_response(
    responses: &[serde_json::Value],
) -> (u16, HashMap<String, String>, Option<serde_json::Value>) {
    if responses.is_empty() {
        return (200, HashMap::new(), None);
    }

    let first = &responses[0];

    // Check if this has an "is" response - this takes priority over proxy
    // Many stubs have "proxy": null alongside "is", so we should use "is" when present
    let has_is_response = first.get("is").is_some();

    // Handle proxy response - only if it's a real proxy config (not null) and there's no "is" response
    if !has_is_response && let Some(proxy) = first.get("proxy") {
        // proxy must be an object with a "to" field to be a real proxy
        if proxy.is_object() && proxy.get("to").is_some() {
            // For proxy, we just verify connectivity - any 2xx is fine, no specific body expected
            return (200, HashMap::new(), None);
        }
    }

    // Handle inject response - expect any response from the JavaScript
    if first.get("inject").is_some() {
        return (200, HashMap::new(), None);
    }

    // Handle fault response
    if let Some(fault) = first.get("fault") {
        // If fault has a specific status, use that
        if let Some(status) = fault.get("status").and_then(|v| v.as_u64()) {
            return (status as u16, HashMap::new(), None);
        }
        // Default fault behavior might return connection errors, but we can expect 500
        return (500, HashMap::new(), None);
    }

    // Handle "is" response format
    if let Some(is_response) = first.get("is") {
        let status = is_response
            .get("statusCode")
            .and_then(|v| {
                // Try as number first, then as string
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(200) as u16;

        let headers = is_response
            .get("headers")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // `_mode:binary` declares a base64 body but the engine serves the decoded bytes, so decode
        // the expected body to match what is served (issue #273).
        let body = is_response.get("body").cloned();
        let body = if is_response.get("_mode").and_then(|v| v.as_str()) == Some("binary") {
            decode_binary_body(body)
        } else {
            body
        };

        return (status, headers, body);
    }

    // Direct format without "is" wrapper
    let status = first
        .get("statusCode")
        .and_then(|v| {
            // Try as number first, then as string
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(200) as u16;

    let headers = first
        .get("headers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let body = first.get("body").cloned();

    (status, headers, body)
}

/// Synthesize a request (method, path, headers, query, body) that satisfies the stub's predicates.
#[allow(clippy::type_complexity)]
pub fn parse_predicates(
    predicates: &[serde_json::Value],
) -> (
    String,
    String,
    HashMap<String, String>,
    HashMap<String, String>,
    Option<String>,
) {
    let mut method = "GET".to_string();
    let mut path = "/".to_string();
    let mut headers = HashMap::new();
    let mut query_params = HashMap::new();
    let mut body = None;
    let mut jsonpath_body: Option<serde_json::Value> = None;

    // First pass: extract startsWith to set base path (regardless of predicate order)
    for predicate in predicates {
        if let Some(starts_with) = predicate.get("startsWith")
            && let Some(p) = starts_with.get("path").and_then(|v| v.as_str())
        {
            path = p.to_string();
        }
    }

    // Second pass: process all other predicates
    for predicate in predicates {
        // Handle jsonpath predicates - build a JSON body based on the selector
        if let Some(jsonpath) = predicate.get("jsonpath")
            && let Some(selector) = jsonpath.get("selector").and_then(|v| v.as_str())
        {
            // Get the expected value from equals.body
            if let Some(equals) = predicate.get("equals")
                && let Some(value) = equals.get("body")
            {
                let json_value = if let Some(s) = value.as_str() {
                    serde_json::Value::String(s.to_string())
                } else {
                    value.clone()
                };

                // Build or merge into jsonpath_body
                let new_obj = build_json_from_jsonpath(selector, json_value);
                jsonpath_body = Some(match jsonpath_body {
                    Some(existing) => merge_json_objects(existing, new_obj),
                    None => new_obj,
                });
            }
        }
        // Handle xpath predicates - build a matching XML body from the selector and the
        // expected `equals.body` value (mirrors the jsonpath handling above). Issue #249 finding #1.
        if let Some(xpath) = predicate.get("xpath")
            && let Some(selector) = xpath.get("selector").and_then(|v| v.as_str())
            && let Some(value) = predicate
                .get("equals")
                .and_then(|e| e.get("body"))
                .and_then(|v| v.as_str())
            && let Some(xml) = build_xml_from_xpath(selector, value)
        {
            body = Some(xml);
        }

        // Handle various predicate formats
        // Note: startsWith is already processed in first pass

        // "equals" predicate - skip body when handled by jsonpath/xpath above
        if let Some(equals) = predicate.get("equals") {
            let skip_body = predicate.get("jsonpath").is_some() || predicate.get("xpath").is_some();
            parse_equals_predicate(
                equals,
                &mut method,
                &mut path,
                &mut headers,
                &mut query_params,
                &mut body,
                skip_body,
            );
        }

        // "matches" predicate (regex - use a sample value)
        if let Some(matches) = predicate.get("matches") {
            if let Some(p) = matches.get("path").and_then(|v| v.as_str()) {
                // Generate a sample path that might match the regex
                path = generate_sample_from_regex(p);
            }
            if let Some(m) = matches.get("method").and_then(|v| v.as_str()) {
                method = generate_sample_from_regex(m);
            }
        }

        // "exists" predicate
        if let Some(exists) = predicate.get("exists") {
            if let Some(hdrs) = exists.get("headers").and_then(|v| v.as_object()) {
                for (name, should_exist) in hdrs {
                    if exists_expects_present(should_exist) {
                        headers.insert(name.clone(), "test-value".to_string());
                    }
                }
            }
            // Synthesize the query param so the request matches (issue #273). The value is
            // non-empty because `exists` reads an empty value as absent (#1403).
            if let Some(q) = exists.get("query").and_then(|v| v.as_object()) {
                for (name, should_exist) in q {
                    if exists_expects_present(should_exist) {
                        query_params.insert(name.clone(), "exists".to_string());
                    }
                }
            }
        }

        // "deepEquals" predicate - skip body when handled by jsonpath/xpath above
        if let Some(deep_equals) = predicate.get("deepEquals") {
            let skip_body = predicate.get("jsonpath").is_some() || predicate.get("xpath").is_some();
            parse_equals_predicate(
                deep_equals,
                &mut method,
                &mut path,
                &mut headers,
                &mut query_params,
                &mut body,
                skip_body,
            );
        }

        // "contains" predicate - processed after base path is set
        if let Some(contains) = predicate.get("contains") {
            parse_contains_predicate(
                contains,
                &mut path,
                &mut headers,
                &mut body,
                &mut query_params,
            );
        }

        // "endsWith" predicate - append to path if needed
        if let Some(ends_with) = predicate.get("endsWith")
            && let Some(p) = ends_with.get("path").and_then(|v| v.as_str())
        {
            // If path doesn't end with the required suffix, append it
            if !path.ends_with(p) {
                if path == "/" {
                    path = format!("/prefix{p}");
                } else if !path.ends_with('/') && !p.starts_with('/') {
                    path = format!("{path}/{p}");
                } else {
                    path = format!("{path}{p}");
                }
            }
        }

        // "and" predicate - recursively parse all inner predicates
        if let Some(and_predicates) = predicate.get("and").and_then(|v| v.as_array()) {
            let inner: Vec<serde_json::Value> = and_predicates.clone();
            let (m, p, h, q, b) = parse_predicates(&inner);
            if m != "GET" {
                method = m;
            }
            if p != "/" {
                path = p;
            }
            headers.extend(h);
            query_params.extend(q);
            if b.is_some() {
                body = b;
            }
        }

        // "or" predicate - use first inner predicate
        if let Some(or_predicates) = predicate.get("or").and_then(|v| v.as_array())
            && let Some(first) = or_predicates.first()
        {
            let inner = vec![first.clone()];
            let (m, p, h, q, b) = parse_predicates(&inner);
            if m != "GET" {
                method = m;
            }
            if p != "/" {
                path = p;
            }
            headers.extend(h);
            query_params.extend(q);
            if b.is_some() {
                body = b;
            }
        }

        // "not" predicate - the stub matches when the inner predicate is FALSE, so build a
        // request that deliberately violates it (issue #249 finding #1).
        if let Some(inner) = predicate.get("not") {
            apply_not_predicate(inner, &mut method, &mut path);
        }
    }

    // If we built a jsonpath body and no explicit body was set, use it
    if body.is_none() && jsonpath_body.is_some() {
        body = jsonpath_body.map(|v| serde_json::to_string(&v).unwrap_or_default());
    }

    (method, path, headers, query_params, body)
}

/// Mutate the working request so it does NOT satisfy `inner` (the body of a `not` predicate).
/// We parse what WOULD satisfy `inner`, then steer each field the inner predicate actually
/// constrains to a guaranteed-different value. Path and method cover the predicates the verifier
/// generates requests from; an inner constraint on body/headers/query is already violated by the
/// default empty request.
/// Whether an `exists` expectation asks for the field, read for JavaScript truthiness as the
/// engine does (#1403): `false`, `null`, `0` and `""` mean absent.
pub fn exists_expects_present(expected: &serde_json::Value) -> bool {
    match expected {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

pub fn apply_not_predicate(inner: &serde_json::Value, method: &mut String, path: &mut String) {
    let (inner_method, _inner_path, _h, _q, _b) = parse_predicates(std::slice::from_ref(inner));
    if predicate_mentions_field(inner, "path") {
        // A sentinel path won't equal / start-with / match a real constrained path (including "/").
        *path = "/__rift_verify_no_match__".to_string();
    }
    if predicate_mentions_field(inner, "method") {
        // Pick any method other than the one the inner predicate constrains (its default is GET).
        *method = if inner_method.eq_ignore_ascii_case("GET") {
            "DELETE".to_string()
        } else {
            "GET".to_string()
        };
    }
}

/// Recursively report whether any object inside `value` carries the key `field` (used to detect
/// which request fields a `not` predicate actually constrains, regardless of operator/nesting).
pub fn predicate_mentions_field(value: &serde_json::Value, field: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(field) || map.values().any(|v| predicate_mentions_field(v, field))
        }
        serde_json::Value::Array(items) => items.iter().any(|v| predicate_mentions_field(v, field)),
        _ => false,
    }
}

/// Unwrap a leading XPath string-ish function (`string(...)`, `boolean(...)`, `number(...)`,
/// `normalize-space(...)`) to the inner location path. Returns the selector unchanged otherwise.
pub fn unwrap_xpath_function(selector: &str) -> &str {
    let s = selector.trim();
    for func in ["string", "boolean", "number", "normalize-space"] {
        if let Some(rest) = s.strip_prefix(func)
            && let Some(inner) = rest
                .trim_start()
                .strip_prefix('(')
                .and_then(|r| r.strip_suffix(')'))
        {
            return inner.trim();
        }
    }
    s
}

/// Clean one location step into a bare XML element name, or `None` if it isn't synthesizable: a
/// non-positional predicate (`[@x='y']`) can't be satisfied by a single element, and anything that
/// isn't a valid XML name (e.g. residue from an unhandled function) is rejected.
pub fn xpath_step_to_tag(step: &str) -> Option<String> {
    let (name, predicate) = match step.find('[') {
        Some(i) => (&step[..i], Some(&step[i..])),
        None => (step, None),
    };
    if let Some(p) = predicate {
        let inner = p.trim_start_matches('[').trim_end_matches(']').trim();
        if inner != "1" {
            return None; // only `[1]` is satisfiable by a single synthesized element
        }
    }
    let name = name.trim();
    let is_xml_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':'));
    is_xml_name.then(|| name.to_string())
}

/// Escape a string for use as XML element text or a double-quoted attribute value.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build a request XML body that satisfies an xpath selector for the given leaf value (issue #249/
/// #261). Handles element paths (`/order/id`), attribute selectors (`//user/@role` → an attribute),
/// and the common function wrappers. Returns `None` for selectors too complex to synthesize, so the
/// caller can emit a visible skip instead of a false failure.
pub fn build_xml_from_xpath(selector: &str, value: &str) -> Option<String> {
    let path = unwrap_xpath_function(selector);
    let steps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (last, ancestors) = steps.split_last()?;
    // Escape so the value survives as XML text/attribute and `string(...)` yields it back verbatim;
    // an unescaped `&`/`<`/`"` would make the body malformed and false-fail the match (issue #261).
    let value = xml_escape(value);

    // A trailing `@attr` becomes an attribute on the preceding element, not a child element.
    let (mut xml, ancestors) = if let Some(attr) = last.strip_prefix('@') {
        let attr = attr.trim();
        let is_attr_name = !attr.is_empty()
            && attr
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':'));
        let (parent, rest) = ancestors.split_last()?;
        let parent = xpath_step_to_tag(parent)?;
        if !is_attr_name {
            return None;
        }
        (format!("<{parent} {attr}=\"{value}\"/>"), rest)
    } else {
        let leaf = xpath_step_to_tag(last)?;
        (format!("<{leaf}>{value}</{leaf}>"), ancestors)
    };

    for step in ancestors.iter().rev() {
        let tag = xpath_step_to_tag(step)?;
        xml = format!("<{tag}>{xml}</{tag}>");
    }
    Some(xml)
}

/// The first xpath selector among `predicates` that drives a body (`equals.body`) but cannot be
/// synthesized into a matching XML request (issue #261). Used to skip such a stub with a reason
/// rather than report a false failure.
pub fn unsynthesizable_xpath(predicates: &[serde_json::Value]) -> Option<String> {
    for predicate in predicates {
        let selector = predicate
            .get("xpath")
            .and_then(|x| x.get("selector"))
            .and_then(|s| s.as_str());
        let has_body = predicate
            .get("equals")
            .and_then(|e| e.get("body"))
            .is_some();
        if let Some(selector) = selector
            && has_body
            && build_xml_from_xpath(selector, "probe").is_none()
        {
            return Some(selector.to_string());
        }
    }
    None
}

/// Build a JSON object from a jsonpath selector and value
/// e.g., "$.receiver.context.correlationKeys.[:0].keyValue" with value "728839"
/// becomes {"receiver":{"context":{"correlationKeys":[{"keyValue":"728839"}]}}}
pub fn build_json_from_jsonpath(selector: &str, value: serde_json::Value) -> serde_json::Value {
    // Remove leading $. if present
    let path = selector.strip_prefix("$.").unwrap_or(selector);

    // Split by . and build nested structure
    let parts: Vec<&str> = path.split('.').collect();

    // Build from inside out
    let mut result = value;

    for part in parts.iter().rev() {
        if part.starts_with("[:") || part.starts_with("[") {
            // Array index like "[:0]" or "[0]" - wrap in array
            result = serde_json::json!([result]);
        } else {
            // Object key
            let mut obj = serde_json::Map::new();
            obj.insert((*part).to_string(), result);
            result = serde_json::Value::Object(obj);
        }
    }

    result
}

/// Merge two JSON objects recursively
pub fn merge_json_objects(
    mut base: serde_json::Value,
    overlay: serde_json::Value,
) -> serde_json::Value {
    if let (serde_json::Value::Object(base_obj), serde_json::Value::Object(overlay_obj)) =
        (&mut base, &overlay)
    {
        for (key, value) in overlay_obj {
            if let Some(existing) = base_obj.get_mut(key) {
                *existing = merge_json_objects(existing.clone(), value.clone());
            } else {
                base_obj.insert(key.clone(), value.clone());
            }
        }
        base
    } else if let (serde_json::Value::Array(base_arr), serde_json::Value::Array(overlay_arr)) =
        (&mut base, &overlay)
    {
        // Merge arrays by extending or merging first elements
        if !overlay_arr.is_empty() {
            if base_arr.is_empty() {
                base_arr.extend(overlay_arr.clone());
            } else {
                // Merge first elements if both are objects
                let merged = merge_json_objects(base_arr[0].clone(), overlay_arr[0].clone());
                base_arr[0] = merged;
            }
        }
        base
    } else {
        overlay
    }
}

pub fn parse_equals_predicate(
    equals: &serde_json::Value,
    method: &mut String,
    path: &mut String,
    headers: &mut HashMap<String, String>,
    query_params: &mut HashMap<String, String>,
    body: &mut Option<String>,
    skip_body: bool,
) {
    if let Some(m) = equals.get("method").and_then(|v| v.as_str()) {
        *method = m.to_string();
    }

    if let Some(p) = equals.get("path").and_then(|v| v.as_str()) {
        *path = p.to_string();
    }

    if let Some(hdrs) = equals.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in hdrs {
            if let Some(v) = value.as_str() {
                headers.insert(name.clone(), v.to_string());
            }
        }
    }

    if let Some(query) = equals.get("query").and_then(|v| v.as_object()) {
        for (name, value) in query {
            if let Some(v) = value.as_str() {
                query_params.insert(name.clone(), v.to_string());
            }
        }
    }

    // Skip body if it's being handled by jsonpath
    if !skip_body && let Some(b) = equals.get("body") {
        if let Some(s) = b.as_str() {
            // Don't set body if it's an empty string (means "body should be absent")
            if !s.is_empty() {
                *body = Some(s.to_string());
            }
        } else {
            *body = Some(serde_json::to_string(b).unwrap_or_default());
        }
    }
}

pub fn parse_contains_predicate(
    contains: &serde_json::Value,
    path: &mut String,
    headers: &mut HashMap<String, String>,
    body: &mut Option<String>,
    query_params: &mut HashMap<String, String>,
) {
    // For "contains", we need to include the substring in our test value
    if let Some(p) = contains.get("path").and_then(|v| v.as_str()) {
        // If path already has a value from startsWith/equals, append to it
        // Otherwise, use the contains value as the path (prefixing / if needed)
        if *path == "/" {
            if p.starts_with('/') {
                *path = p.to_string();
            } else {
                *path = format!("/{p}");
            }
        } else if !path.contains(p) {
            // Append the contains substring to the existing path if not already present
            // Add a slash separator if needed
            if !path.ends_with('/') && !p.starts_with('/') {
                path.push('/');
            }
            path.push_str(p);
        }
    }

    // Handle query parameters in contains
    if let Some(query) = contains.get("query").and_then(|v| v.as_object()) {
        for (name, value) in query {
            if let Some(v) = value.as_str() {
                // For contains, include the substring in the query value
                query_params.insert(name.clone(), v.to_string());
            }
        }
    }

    if let Some(hdrs) = contains.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in hdrs {
            if let Some(v) = value.as_str() {
                headers.insert(name.clone(), format!("prefix{v}suffix"));
            }
        }
    }

    if let Some(b) = contains.get("body").and_then(|v| v.as_str()) {
        // Append to existing body if present (handles multiple contains predicates)
        if let Some(existing) = body {
            *body = Some(format!("{existing} {b}"));
        } else {
            *body = Some(format!("test {b} content"));
        }
    }
}

/// Strip a leading inline-flag group like `(?i)` / `(?ims)` (issue #273) so the remaining pattern
/// can be sampled into a literal — the global flags don't change the generated sample. A scoped
/// group such as `(?:...)` or `(?i:...)` (flags followed by `:`) is left intact.
pub fn strip_leading_inline_flags(pattern: &str) -> &str {
    if let Some(rest) = pattern.strip_prefix("(?")
        && let Some(close) = rest.find(')')
    {
        let flags = &rest[..close];
        if !flags.is_empty() && flags.chars().all(|c| c.is_ascii_alphabetic()) {
            return &rest[close + 1..];
        }
    }
    pattern
}

pub fn generate_sample_from_regex(pattern: &str) -> String {
    // Simple heuristic to generate a sample that might match common patterns
    // This is a best-effort approach for common regex patterns
    let pattern = strip_leading_inline_flags(pattern);

    // /api/v\d+/users -> /api/v1/users
    // Important: Replace character class patterns BEFORE stripping anchors,
    // since [^/]+ contains ^ as negation, not as anchor
    let sample = pattern
        // Replace character classes first (before anchor removal)
        .replace(r"[^/]+", "item")
        .replace(r"[a-zA-Z]+", "test")
        .replace(r"[0-9]+", "123")
        .replace(r"[a-z]+", "test")
        .replace(r"[A-Z]+", "TEST")
        // Replace other common patterns
        .replace(r"\d+", "1")
        .replace(r"\d", "1")
        .replace(r"\w+", "test")
        .replace(r"\w", "a")
        .replace(r".*", "")
        .replace(r".+", "x");

    // Strip anchors only at start/end of string
    let sample = sample.strip_prefix('^').unwrap_or(&sample).to_string();
    let sample = sample.strip_suffix('$').unwrap_or(&sample).to_string();

    if sample.is_empty() {
        "/".to_string()
    } else {
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── #1: not predicate ──────────────────────────────────────────────────
    #[test]
    fn not_predicate_generates_non_matching_path() {
        let preds = vec![json!({ "not": { "equals": { "path": "/secret" } } })];
        let (_method, path, _h, _q, _b) = parse_predicates(&preds);
        assert_ne!(
            path, "/secret",
            "generated path must NOT satisfy the inner equals"
        );
    }

    #[test]
    fn not_predicate_flips_method() {
        let preds = vec![json!({ "not": { "equals": { "method": "POST" } } })];
        let (method, _p, _h, _q, _b) = parse_predicates(&preds);
        assert_ne!(
            method, "POST",
            "generated method must NOT satisfy the inner equals"
        );
    }

    #[test]
    fn not_predicate_steers_away_from_default_method_and_path() {
        // The forbidden value equals the parser default (GET / "/"), so a naive `!= default`
        // guard would leave the request satisfying the inner predicate — it must still be steered.
        let (method, _p, _h, _q, _b) =
            parse_predicates(&[json!({ "not": { "equals": { "method": "GET" } } })]);
        assert_ne!(method, "GET");
        let (_m, path, _h, _q, _b) =
            parse_predicates(&[json!({ "not": { "equals": { "path": "/" } } })]);
        assert_ne!(path, "/");
    }

    // ── #1: xpath predicate ────────────────────────────────────────────────
    #[test]
    fn build_xml_from_xpath_nests_elements() {
        assert_eq!(
            build_xml_from_xpath("/order/id", "728839").as_deref(),
            Some("<order><id>728839</id></order>")
        );
    }

    #[test]
    fn build_xml_from_xpath_single_element() {
        assert_eq!(
            build_xml_from_xpath("/root", "x").as_deref(),
            Some("<root>x</root>")
        );
    }

    #[test]
    fn build_xml_from_xpath_builds_attribute() {
        // `//user/@role` selects an attribute → it must be an attribute, not a child element.
        assert_eq!(
            build_xml_from_xpath("//user/@role", "admin").as_deref(),
            Some("<user role=\"admin\"/>")
        );
        // Attribute under a nested element keeps the ancestors.
        assert_eq!(
            build_xml_from_xpath("/order/item/@id", "7").as_deref(),
            Some("<order><item id=\"7\"/></order>")
        );
    }

    #[test]
    fn build_xml_from_xpath_unwraps_function() {
        // `string(...)` / `number(...)` / `normalize-space(...)` wrappers are unwrapped to the path.
        assert_eq!(
            build_xml_from_xpath("string(//user/@role)", "admin").as_deref(),
            Some("<user role=\"admin\"/>")
        );
        assert_eq!(
            build_xml_from_xpath("number(/order/id)", "7").as_deref(),
            Some("<order><id>7</id></order>")
        );
        assert_eq!(
            build_xml_from_xpath("normalize-space(/a/b)", "v").as_deref(),
            Some("<a><b>v</b></a>")
        );
    }

    #[test]
    fn build_xml_from_xpath_positional_predicate() {
        // Only `[1]` is satisfiable by a single synthesized element; `[2]` would never match → None.
        assert_eq!(
            build_xml_from_xpath("/order/item[1]/id", "7").as_deref(),
            Some("<order><item><id>7</id></item></order>")
        );
        assert_eq!(build_xml_from_xpath("/order/item[2]/id", "7"), None);
    }

    #[test]
    fn build_xml_from_xpath_escapes_value() {
        // XML metacharacters in the value must be escaped (attribute and element-text contexts), so
        // the body stays well-formed and `string(...)` yields the original value back.
        assert_eq!(
            build_xml_from_xpath("//user/@role", "a&b\"c").as_deref(),
            Some("<user role=\"a&amp;b&quot;c\"/>")
        );
        assert_eq!(
            build_xml_from_xpath("/note/body", "x < y & z").as_deref(),
            Some("<note><body>x &lt; y &amp; z</body></note>")
        );
    }

    #[test]
    fn build_xml_from_xpath_none_when_unsynthesizable() {
        // Unhandled function, value-filter predicate, and empty path can't be synthesized → None.
        assert_eq!(build_xml_from_xpath("count(//x)", "5"), None);
        assert_eq!(build_xml_from_xpath("//user[@role='admin']", "x"), None);
        assert_eq!(build_xml_from_xpath("string()", "x"), None);
    }

    #[test]
    fn xpath_builds_matching_xml_body() {
        let preds = vec![json!({
            "xpath": { "selector": "/order/id" },
            "equals": { "body": "728839" }
        })];
        let (_m, _p, _h, _q, body) = parse_predicates(&preds);
        assert_eq!(body.as_deref(), Some("<order><id>728839</id></order>"));
    }

    #[test]
    fn xpath_attribute_selector_builds_matching_body() {
        // The conformance sample 02 stub #11 shape.
        let preds = vec![json!({
            "xpath": { "selector": "string(//user/@role)" },
            "equals": { "body": "admin" }
        })];
        let (_m, _p, _h, _q, body) = parse_predicates(&preds);
        assert_eq!(body.as_deref(), Some("<user role=\"admin\"/>"));
    }

    #[test]
    fn unsynthesizable_xpath_stub_skipped() {
        let stub: Stub = serde_json::from_value(json!({
            "predicates": [{ "xpath": { "selector": "count(//x)" }, "equals": { "body": "5" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "ok" } }]
        }))
        .unwrap();
        let cases = generate_test_cases(0, &stub, false, "http", None);
        let reason = cases[0].skip_reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("xpath") && reason.contains("count(//x)"),
            "reason: {reason}"
        );
    }

    // ── #3: tcp fault detection + classification ───────────────────────────
    #[test]
    fn expects_tcp_fault_detects_rift_fault_tcp() {
        let responses = vec![json!({ "_rift": { "fault": { "tcp": "reset" } } })];
        assert!(expects_tcp_fault(&responses));
    }

    #[test]
    fn expects_tcp_fault_false_for_normal_and_latency() {
        assert!(!expects_tcp_fault(&[
            json!({ "is": { "statusCode": 200 } })
        ]));
        assert!(!expects_tcp_fault(&[
            json!({ "_rift": { "fault": { "latency": 50 } } })
        ]));
    }

    // Issue #531: the certain object form (p >= 1.0) still expects a transport error, but the
    // probabilistic form (p < 1.0) is non-deterministic and must not be asserted as a reset.
    #[test]
    fn expects_tcp_fault_certainty() {
        assert!(expects_tcp_fault(&[json!({
            "_rift": { "fault": { "tcp": { "probability": 1.0, "type": "reset" } } }
        })]));
        assert!(!expects_tcp_fault(&[json!({
            "_rift": { "fault": { "tcp": { "probability": 0.1, "type": "reset" } } }
        })]));
    }

    #[test]
    fn generate_sample_from_regex_strips_inline_flags() {
        // A leading inline-flag group `(?i)` must not leak into the synthesized path.
        assert_eq!(generate_sample_from_regex("(?i)^/case$"), "/case");
        // Existing behavior preserved.
        assert_eq!(generate_sample_from_regex("^/id/[0-9]+$"), "/id/123");
    }

    #[test]
    fn parse_predicates_synthesizes_exists_query() {
        let preds = vec![serde_json::json!({"exists": {"query": {"flag": true}}})];
        let (_method, _path, _headers, query, _body) = parse_predicates(&preds);
        assert!(
            query.contains_key("flag"),
            "exists-query must synthesize the param: {query:?}"
        );
    }

    #[test]
    fn extract_expected_response_decodes_binary_mode() {
        // `_mode:binary` declares a base64 body; decode it so it matches the served bytes.
        let responses = vec![
            serde_json::json!({"is": {"statusCode": 200, "body": "aGVsbG8=", "_mode": "binary"}}),
        ];
        let (status, _headers, body) = extract_expected_response(&responses);
        assert_eq!(status, 200);
        assert_eq!(body, Some(serde_json::Value::String("hello".to_string())));
    }

    #[test]
    fn extract_expected_response_binary_invalid_base64_keeps_raw() {
        // Invalid base64 falls back to the raw string (mirrors the engine's decode-or-raw).
        let responses = vec![
            serde_json::json!({"is": {"statusCode": 200, "body": "!!!notbase64", "_mode": "binary"}}),
        ];
        let (_status, _headers, body) = extract_expected_response(&responses);
        assert_eq!(
            body,
            Some(serde_json::Value::String("!!!notbase64".to_string()))
        );
    }

    #[test]
    fn strip_leading_inline_flags_leaves_scoped_groups_intact() {
        // Only a global flag group like `(?i)` is stripped; non-capturing/scoped/lookaround
        // constructs must be preserved (issue #273).
        assert_eq!(generate_sample_from_regex("(?:abc)"), "(?:abc)");
        assert_eq!(generate_sample_from_regex("(?i:case)"), "(?i:case)");
        assert!(generate_sample_from_regex("(?=/x)/y").starts_with("(?="));
    }

    #[test]
    fn parse_predicates_exists_query_false_omits_param() {
        let preds = vec![serde_json::json!({"exists": {"query": {"flag": false}}})];
        let (_method, _path, _headers, query, _body) = parse_predicates(&preds);
        assert!(
            !query.contains_key("flag"),
            "exists:false must not synthesize the param: {query:?}"
        );
    }

    #[test]
    fn test_case_carries_stub_space() {
        // Issue #260: a space-gated stub must drive requests with its own space, not the global one.
        let with_space: Stub = serde_json::from_value(json!({
            "space": "alice",
            "predicates": [{ "equals": { "path": "/data" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "A" } }]
        }))
        .unwrap();
        let cases = generate_test_cases(0, &with_space, false, "http", Some("X-Mock-Space"));
        assert_eq!(cases[0].flow_space.as_deref(), Some("alice"));

        let no_space: Stub = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/data" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "A" } }]
        }))
        .unwrap();
        let cases = generate_test_cases(0, &no_space, false, "http", Some("X-Mock-Space"));
        assert_eq!(cases[0].flow_space, None);
    }

    #[test]
    fn space_stub_skipped_when_flow_header_unresolved() {
        let stub: Stub = serde_json::from_value(json!({
            "space": "alice",
            "predicates": [{ "equals": { "path": "/data" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "A" } }]
        }))
        .unwrap();
        // No flow header resolved → the space-gated stub is a visible SKIP, not a silent degraded run.
        let cases = generate_test_cases(0, &stub, false, "http", None);
        assert!(
            cases[0]
                .skip_reason
                .as_deref()
                .unwrap_or("")
                .contains("flowIdSource")
        );
        // With a header resolved, it is verified normally (no skip).
        let cases = generate_test_cases(0, &stub, false, "http", Some("X-Mock-Space"));
        assert!(cases[0].skip_reason.is_none());
    }

    #[test]
    fn test_cases_for_stubs_keeps_document_order_and_skips_dynamic() {
        let stubs = vec![
            json!({
                "predicates": [{ "equals": { "path": "/a" } }],
                "responses": [{ "is": { "statusCode": 201, "body": "A" } }]
            }),
            json!({
                "predicates": [{ "equals": { "path": "/b" } }],
                "responses": [{ "inject": "function() {}" }]
            }),
        ];
        let cases = test_cases_for_stubs(&stubs, "http", None).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!((cases[0].stub_index, cases[0].path.as_str()), (0, "/a"));
        assert_eq!(cases[0].expected_status, 201);
        assert!(cases[0].skip_reason.is_none());
        assert_eq!(cases[1].stub_index, 1);
        assert!(cases[1].skip_reason.is_some());
        assert!(test_cases_for_stubs(&[json!("nope")], "http", None).is_err());
    }
}
