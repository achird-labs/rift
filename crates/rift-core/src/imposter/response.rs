//! Response building and execution logic for imposters.
//!
//! This module handles creating responses from stubs, applying behaviors,
//! and managing the response cycle.

use super::types::{
    DebugResponsePreview, IsResponse, ResponseMode, RiftResponseExtension, RiftScriptConfig,
    StubResponse,
};
use crate::behaviors::{
    DecorateError, HasRepeatBehavior, RequestContext, apply_decorate, is_js_config_decorate,
    rewrite_js_config_to_rhai,
};
use crate::imposter::Predicate;
use std::collections::HashMap;

/// Truncate a string with ellipsis if it exceeds the maximum byte length.
///
/// This function is unicode-safe and will not panic on multi-byte characters.
/// It finds the nearest valid UTF-8 character boundary at or before `max_len`.
fn truncate_with_ellipsis(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }

    let end = text.floor_char_boundary(max_len);
    format!("{}...", &text[..end])
}

// Implement HasRepeatBehavior for StubResponse
impl HasRepeatBehavior for StubResponse {
    fn get_repeat(&self) -> Option<u32> {
        match self {
            StubResponse::Is { behaviors, .. } => behaviors
                .as_ref()
                .and_then(|b| b.get("repeat"))
                .and_then(|r| r.as_u64())
                .map(|r| r as u32),
            StubResponse::RiftScript { .. } => None,
            _ => None,
        }
    }
}

/// Create response preview from a StubResponse (for debug mode)
pub fn create_response_preview(response: &StubResponse) -> DebugResponsePreview {
    match response {
        StubResponse::Is { is, .. } => {
            let body_preview = is.body.as_ref().map(|b| match b {
                serde_json::Value::String(s) => truncate_with_ellipsis(s, 500),
                other => {
                    let json = serde_json::to_string(other).unwrap_or_default();
                    truncate_with_ellipsis(&json, 500)
                }
            });
            let headers = if is.headers.is_empty() {
                None
            } else {
                Some(
                    is.headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.join(", ")))
                        .collect(),
                )
            };
            DebugResponsePreview {
                response_type: "is".to_string(),
                status_code: Some(is.status_code),
                headers,
                body_preview,
            }
        }
        StubResponse::Proxy { proxy, .. } => DebugResponsePreview {
            response_type: "proxy".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!("Proxy to: {}", proxy.to)),
        },
        StubResponse::Inject { inject, .. } => DebugResponsePreview {
            response_type: "inject".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!(
                "JavaScript inject: {}",
                truncate_with_ellipsis(inject, 50)
            )),
        },
        StubResponse::Fault { fault, .. } => DebugResponsePreview {
            response_type: "fault".to_string(),
            status_code: None,
            headers: None,
            body_preview: Some(format!("Fault: {fault}")),
        },
        StubResponse::RiftScript { rift } => {
            // RiftScript uses the _rift extension namespace
            let script_info = if rift.script.is_some() {
                "Rift script response"
            } else if rift.fault.is_some() {
                "Rift fault injection"
            } else {
                "Rift extension response"
            };
            DebugResponsePreview {
                response_type: "_rift".to_string(),
                status_code: None,
                headers: None,
                body_preview: Some(script_info.to_string()),
            }
        }
    }
}

/// Execute a stub response with Rift extensions
/// Returns (status, headers, body, behaviors, rift_extension, response_mode, is_fault)
#[allow(clippy::type_complexity)]
pub fn execute_stub_response_with_rift(
    response: &StubResponse,
) -> Option<(
    u16,
    HashMap<String, Vec<String>>,
    String,
    Option<serde_json::Value>,
    Option<RiftResponseExtension>,
    ResponseMode,
    bool,
)> {
    match response {
        StubResponse::Is {
            is,
            behaviors,
            rift,
        } => {
            let mut headers = is.headers.clone();
            let mode = is.mode.clone();

            let body = is
                .body
                .as_ref()
                .map(|b| {
                    if b.is_string() {
                        b.as_str().unwrap_or("").to_string()
                    } else {
                        if !headers.contains_key("content-type")
                            && !headers.contains_key("Content-Type")
                        {
                            headers.insert(
                                "Content-Type".to_string(),
                                vec!["application/json".to_string()],
                            );
                        }
                        serde_json::to_string(b).unwrap_or_default()
                    }
                })
                .unwrap_or_default();

            Some((
                is.status_code,
                headers,
                body,
                behaviors.clone(),
                rift.clone(),
                mode,
                false,
            ))
        }
        StubResponse::Fault { fault } => Some((
            0,
            HashMap::new(),
            fault.clone(),
            None,
            None,
            ResponseMode::Text,
            true,
        )),
        StubResponse::Proxy { .. } => None,
        StubResponse::Inject { .. } => None,
        StubResponse::RiftScript { .. } => None,
    }
}

/// Get RiftScript config if the response is a RiftScript type
pub fn get_rift_script_config(response: &StubResponse) -> Option<RiftScriptConfig> {
    match response {
        StubResponse::RiftScript { rift } => rift.script.clone(),
        _ => None,
    }
}

/// Create a stub from a recorded proxy response.
///
/// If the body is valid UTF-8, it is stored as text (JSON or string).
/// If the body is not valid UTF-8 (binary content), it is base64-encoded
/// and the stub uses `_mode: "binary"` so it can be replayed correctly.
///
/// Headers are accepted as `&[(String, String)]` to preserve multi-valued
/// headers (e.g., multiple `Set-Cookie`), which are stored as separate values in
/// the stub's `IsResponse` (multi-value headers, issue #238). Hop-by-hop headers
/// are dropped.
pub fn create_stub_from_proxy_response(
    predicates: Vec<serde_json::Value>,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    latency_ms: Option<u64>,
    decorate_fn: Option<String>,
    recorded_from: Option<String>,
) -> super::types::Stub {
    // Group values per key so multiple values for one header (e.g. Set-Cookie) survive replay.
    let response_headers: HashMap<String, Vec<String>> = {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in headers {
            if !crate::util::is_hop_by_hop_header(k) {
                map.entry(k.clone()).or_default().push(v.clone());
            }
        }
        map
    };

    let (body_value, is_binary) = crate::util::encode_body_for_stub(body);
    let mode = if is_binary {
        ResponseMode::Binary
    } else {
        ResponseMode::Text
    };

    let is_response = IsResponse {
        status_code: status,
        headers: response_headers,
        body: body_value,
        mode,
    };

    // Build behaviors object if needed
    let behaviors = if latency_ms.is_some() || decorate_fn.is_some() {
        let mut behaviors_obj = serde_json::Map::new();
        if let Some(ms) = latency_ms {
            behaviors_obj.insert("wait".to_string(), serde_json::json!(ms));
        }
        if let Some(fn_str) = decorate_fn {
            behaviors_obj.insert("decorate".to_string(), serde_json::json!(fn_str));
        }
        Some(serde_json::Value::Object(behaviors_obj))
    } else {
        None
    };

    let predicates: Vec<Predicate> = predicates
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value.clone()) {
            Ok(pred) => Some(pred),
            Err(e) => {
                tracing::warn!(
                    "Skipping malformed generated predicate: {} (from: {})",
                    e,
                    value
                );
                None
            }
        })
        .collect();
    super::types::Stub {
        id: None,
        route_pattern: None,
        predicates,
        responses: vec![StubResponse::Is {
            is: is_response,
            behaviors,
            rift: None,
        }],
        scenario_name: None,
        required_scenario_state: None,
        new_scenario_state: None,
        space: None,
        recorded_from,
        verify: None,
    }
}

/// Apply decorate behavior - handles both JavaScript and Rhai scripts
pub fn apply_js_or_rhai_decorate(
    script: &str,
    request: &RequestContext,
    body: &str,
    status: u16,
    headers: &mut HashMap<String, String>,
) -> Result<(String, u16), DecorateError> {
    // Mountebank's JS `config =>` convention (issue #191): rewrite simple field access onto the
    // Rhai request/response maps. Checked before the `function` route since arrow scripts don't
    // start with "function" and `function(config)` uses the config model, not (request, response).
    if is_js_config_decorate(script) {
        // Issue #305: a `config =>` decorate that `require()`s an external CommonJS module
        // can't be represented by the lossy Rhai rewrite below (it doesn't run real JS), so
        // route it through the Boa engine instead, where `require()` actually works.
        #[cfg(feature = "javascript")]
        // `require(` / `require (` → needs the real JS engine (not the lossy Rhai rewrite). A false
        // positive only routes a simple decorate through Boa (still correct); a false negative would
        // fail loudly at Rhai-eval, never silently.
        if script.contains("require(") || script.contains("require (") {
            let mb_request = crate::scripting::MountebankRequest {
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            };
            return match crate::scripting::execute_mountebank_config_decorate(
                script,
                &mb_request,
                body,
                status,
                headers,
            ) {
                Ok(result) => {
                    for (k, v) in result.headers {
                        headers.insert(k, v);
                    }
                    Ok((result.body, result.status_code))
                }
                Err(e) => Err(DecorateError::JavaScript(e.to_string())),
            };
        }

        let rhai_script = rewrite_js_config_to_rhai(script);
        return apply_decorate(&rhai_script, request, body, status, headers);
    }

    // Check if it's a JavaScript function declaration
    if script.trim().starts_with("function") {
        #[cfg(feature = "javascript")]
        {
            // Use the JavaScript engine for proper execution
            let mb_request = crate::scripting::MountebankRequest {
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            };

            match crate::scripting::execute_mountebank_decorate(
                script,
                &mb_request,
                body,
                status,
                headers,
            ) {
                Ok(result) => {
                    // Update headers from the result
                    for (k, v) in result.headers {
                        headers.insert(k, v);
                    }
                    Ok((result.body, result.status_code))
                }
                Err(e) => Err(DecorateError::JavaScript(e.to_string())),
            }
        }

        #[cfg(not(feature = "javascript"))]
        {
            // Fallback to Rhai conversion when JavaScript feature is disabled
            if let Some(start) = script.find('{') {
                if let Some(end) = script.rfind('}') {
                    let js_body = script[start + 1..end].trim();
                    let rhai_script = js_body.replace('\'', "\"");
                    return apply_decorate(&rhai_script, request, body, status, headers);
                }
            }
            Err(DecorateError::JsParseFailure)
        }
    } else {
        // Assume it's Rhai script
        apply_decorate(script, request, body, status, headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #191: the JS `config =>` decorate convention runs (rewritten to Rhai) end-to-end.
    fn decorate_req() -> RequestContext {
        RequestContext {
            method: "GET".to_string(),
            path: "/orders".to_string(),
            query: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            body: Some("REQ-BODY".to_string()),
        }
    }

    #[test]
    fn decorate_js_config_sets_body() {
        let mut headers = std::collections::HashMap::new();
        let (body, status) = apply_js_or_rhai_decorate(
            "config => { config.response.body = 'hello'; }",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(body, "hello");
        assert_eq!(status, 200);
    }

    #[test]
    fn decorate_js_config_reads_request_body() {
        let mut headers = std::collections::HashMap::new();
        let (body, _) = apply_js_or_rhai_decorate(
            "config => { config.response.body = config.request.body; }",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(body, "REQ-BODY");
    }

    // Issue #305: a `config =>` decorate that require()s an external module must route to the JS
    // engine (not the lossy Rhai rewrite) and actually run the module.
    #[cfg(feature = "javascript")]
    #[test]
    fn decorate_js_config_require_routes_to_boa() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let module =
            std::env::temp_dir().join(format!("rift_305_route_{}_{n}.cjs", std::process::id()));
        std::fs::write(
            &module,
            "module.exports = function (config) { config.response.body = 'REQUIRE-RAN'; };\n",
        )
        .unwrap();
        let script = format!(
            "config => {{ const s = require('{}'); s(config); }}",
            module.display()
        );
        let mut headers = std::collections::HashMap::new();
        let result =
            apply_js_or_rhai_decorate(&script, &decorate_req(), "original", 200, &mut headers);
        let _ = std::fs::remove_file(&module);
        let (body, _) = result.expect("require-based config decorate should run");
        assert_eq!(body, "REQUIRE-RAN");
    }

    #[test]
    fn decorate_js_config_sets_status() {
        let mut headers = std::collections::HashMap::new();
        let (_, status) = apply_js_or_rhai_decorate(
            "config => { config.response.statusCode = 404; }",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(status, 404);
    }

    #[test]
    fn decorate_js_config_function_wrapper_executes() {
        // `function(config) { ... }` must take the config-rewrite route, NOT the `function`→JS-engine
        // route (it uses the config model, not (request, response)). Locks the detection ordering.
        let mut headers = std::collections::HashMap::new();
        let (body, _) = apply_js_or_rhai_decorate(
            "function(config) { config.response.body = 'fn'; }",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(body, "fn");
    }

    #[test]
    fn decorate_js_config_no_wrapper_executes() {
        // A bare body (no arrow/function wrapper) is detected via `config.response.` and rewritten
        // in place (no brace stripping).
        let mut headers = std::collections::HashMap::new();
        let (body, _) = apply_js_or_rhai_decorate(
            "config.response.body = 'bare';",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(body, "bare");
    }

    #[test]
    fn decorate_plain_rhai_still_works() {
        let mut headers = std::collections::HashMap::new();
        let (body, _) = apply_js_or_rhai_decorate(
            "response.body = request.path;",
            &decorate_req(),
            "original",
            200,
            &mut headers,
        )
        .unwrap();
        assert_eq!(body, "/orders", "existing Rhai decorate must be unchanged");
    }

    // =========================================================================
    // Issue #116: Multi-valued headers preserved via create_stub_from_proxy_response
    // =========================================================================

    #[test]
    fn test_create_stub_preserves_multi_valued_headers() {
        // Multiple Set-Cookie headers are preserved as separate values in the stub (issue #238)
        let headers = vec![
            ("Set-Cookie".to_string(), "session=abc".to_string()),
            ("Set-Cookie".to_string(), "theme=dark".to_string()),
            ("Content-Type".to_string(), "text/html".to_string()),
        ];

        let stub = create_stub_from_proxy_response(vec![], 200, &headers, b"OK", None, None, None);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                let cookies = is.headers.get("Set-Cookie").unwrap();
                assert_eq!(
                    cookies,
                    &vec!["session=abc".to_string(), "theme=dark".to_string()],
                    "Multi-valued Set-Cookie headers are preserved as separate values"
                );
                assert_eq!(
                    is.headers.get("Content-Type").unwrap(),
                    &vec!["text/html".to_string()]
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_hop_by_hop_headers_filtered() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            ("Keep-Alive".to_string(), "timeout=5".to_string()),
        ];

        let stub = create_stub_from_proxy_response(vec![], 200, &headers, b"OK", None, None, None);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert!(is.headers.contains_key("Content-Type"));
                assert!(
                    !is.headers.contains_key("Transfer-Encoding"),
                    "Transfer-Encoding should be filtered"
                );
                assert!(
                    !is.headers.contains_key("Connection"),
                    "Connection should be filtered"
                );
                assert!(
                    !is.headers.contains_key("Keep-Alive"),
                    "Keep-Alive should be filtered"
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    // =========================================================================
    // Issue #117: Binary response bodies correctly base64-encoded
    // =========================================================================

    #[test]
    fn test_create_stub_binary_body_base64_encoded() {
        // Non-UTF-8 bytes should be base64-encoded with binary mode
        let binary_body: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0xFD, 0x89, 0x50, 0x4E, 0x47];

        let stub =
            create_stub_from_proxy_response(vec![], 200, &[], &binary_body, None, None, None);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Binary, "Binary body should set mode");

                // Verify the body is base64
                use base64::Engine;
                let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&binary_body);
                assert_eq!(
                    is.body.as_ref().unwrap().as_str().unwrap(),
                    expected_b64,
                    "Binary body should be base64-encoded"
                );
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_text_body_not_base64() {
        let stub =
            create_stub_from_proxy_response(vec![], 200, &[], b"Hello, World!", None, None, None);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(
                    is.mode,
                    ResponseMode::Text,
                    "Text body should use text mode"
                );
                assert_eq!(is.body.as_ref().unwrap().as_str().unwrap(), "Hello, World!");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_json_body_parsed() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            br#"{"key": "value"}"#,
            None,
            None,
            None,
        );

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Text);
                // JSON bodies are parsed into serde_json::Value, not stored as strings
                let body = is.body.as_ref().unwrap();
                assert!(body.is_object(), "JSON body should be parsed as object");
                assert_eq!(body["key"], "value");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_empty_body() {
        let stub = create_stub_from_proxy_response(vec![], 204, &[], b"", None, None, None);

        match &stub.responses[0] {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.mode, ResponseMode::Text);
                assert!(is.body.is_none(), "Empty body should be None");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    #[test]
    fn test_create_stub_with_latency_and_decorate() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            b"OK",
            Some(150),
            Some("function(request, response) {}".to_string()),
            None,
        );

        match &stub.responses[0] {
            StubResponse::Is { behaviors, .. } => {
                let b = behaviors.as_ref().unwrap();
                assert_eq!(b["wait"], 150);
                assert_eq!(b["decorate"], "function(request, response) {}");
            }
            _ => panic!("Expected StubResponse::Is"),
        }
    }

    // =========================================================================
    // Truncation tests
    // =========================================================================

    #[test]
    fn test_truncate_with_ellipsis_short_string() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_with_ellipsis_exact_length() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_with_ellipsis_long_string() {
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_with_ellipsis_unicode_safe() {
        // "日本語" is 9 bytes (3 bytes per character)
        // Truncating at byte 5 would be mid-character
        // floor_char_boundary(5) returns 3 (end of first char)
        let text = "日本語";
        assert_eq!(text.len(), 9);
        assert_eq!(truncate_with_ellipsis(text, 5), "日...");
    }

    #[test]
    fn test_truncate_with_ellipsis_emoji() {
        // Each emoji is 4 bytes
        // floor_char_boundary(5) returns 4 (end of first emoji)
        let text = "👋🌍🎉";
        assert_eq!(truncate_with_ellipsis(text, 5), "👋...");
    }

    #[test]
    fn test_truncate_with_ellipsis_mixed_content() {
        // "Hello " is 6 bytes, "世" is 3 bytes, "界" is 3 bytes, "!" is 1 byte = 13 bytes
        // floor_char_boundary(8) returns 6 (byte 8 is mid-character of "世")
        let text = "Hello 世界!";
        assert_eq!(truncate_with_ellipsis(text, 8), "Hello ...");
    }

    #[test]
    fn test_truncate_with_ellipsis_empty_string() {
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn test_truncate_with_ellipsis_zero_max_len() {
        assert_eq!(truncate_with_ellipsis("hello", 0), "...");
    }

    // Issue #119: Malformed predicates are skipped instead of panicking
    #[test]
    fn test_create_stub_malformed_predicate_skipped() {
        // A valid predicate alongside a completely invalid one
        let valid_predicate = serde_json::json!({
            "equals": { "method": "GET" }
        });
        // This is not a valid Predicate shape — should be skipped via filter_map
        let malformed_predicate = serde_json::json!({
            "notARealPredicate": { "foo": "bar" }
        });

        let stub = create_stub_from_proxy_response(
            vec![valid_predicate, malformed_predicate],
            200,
            &[],
            b"OK",
            None,
            None,
            None,
        );

        // The malformed predicate should be silently skipped
        assert_eq!(stub.predicates.len(), 1);
    }

    #[test]
    fn test_create_stub_all_predicates_malformed() {
        // All predicates are invalid — stub should have zero predicates
        let bad1 = serde_json::json!({"garbage": 123});
        let bad2 = serde_json::json!("just a string");

        let stub =
            create_stub_from_proxy_response(vec![bad1, bad2], 200, &[], b"OK", None, None, None);

        assert!(stub.predicates.is_empty());
    }

    #[test]
    fn test_create_stub_recorded_from_populated() {
        let stub = create_stub_from_proxy_response(
            vec![],
            200,
            &[],
            b"OK",
            None,
            None,
            Some("http://upstream:8080".to_string()),
        );
        assert_eq!(stub.recorded_from.as_deref(), Some("http://upstream:8080"));
    }

    #[test]
    fn test_create_stub_recorded_from_none_when_not_provided() {
        let stub = create_stub_from_proxy_response(vec![], 200, &[], b"OK", None, None, None);
        assert!(stub.recorded_from.is_none());
    }
}
