//! Issue #359 — declarative response templating (`_rift.templated`).
//!
//! Runs with `RIFT_DEBUG` unset, so the non-debug error policy (empty-string substitution + a
//! `tracing::warn!`) applies for the whole binary; the debug-mode (request-time error) side of
//! the policy is covered separately in `issue_359_response_templating_debug.rs`, which needs its
//! own process so the `RIFT_DEBUG` `OnceLock` caches the opposite value.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("valid imposter config")
}

async fn spawn(manager: &ImposterManager, config: serde_json::Value) {
    manager
        .create_imposter(cfg(config))
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// Opt-in gate: `templated` absent/false must leave a literal `{{...}}` untouched (so a recorded
/// fixture containing `{{` is served verbatim), even though the token uses the new grammar.
#[tokio::test]
async fn templated_false_leaves_new_grammar_tokens_untouched() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20059, "protocol": "http",
            "stubs": [{
                "responses": [{ "is": { "statusCode": 200, "body": "{{request.method}} and {{now}}" } }]
            }]
        }),
    )
    .await;

    let body = reqwest::get("http://127.0.0.1:20059/x")
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert_eq!(
        body, "{{request.method}} and {{now}}",
        "templated:false must serve the body verbatim"
    );
    let _ = manager.delete_imposter(20059).await;
}

/// The legacy `{{NOW}}`/`{{DAYS+N}}` date tokens keep expanding unconditionally on the `Text`
/// response path regardless of `templated` (issue #359 constraint: don't regress issue #195).
#[tokio::test]
async fn legacy_date_tokens_unaffected_by_templated_flag() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20060, "protocol": "http",
            "stubs": [{
                "responses": [{ "is": { "statusCode": 200, "body": "{{NOW}}" } }]
            }]
        }),
    )
    .await;

    let body = reqwest::get("http://127.0.0.1:20060/x")
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    chrono::DateTime::parse_from_rfc3339(&body).expect("{{NOW}} still expands when untemplated");
    let _ = manager.delete_imposter(20060).await;
}

/// `templated: true` evaluates `request.method`/`request.query.X`/`request.header` in the body
/// AND in a response header (issue #359 AC: headers are templated too).
#[tokio::test]
async fn templated_true_expands_request_accessors_in_body_and_headers() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20061, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/greet" } }],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "headers": { "X-Echo-Method": "{{request.method}}" },
                        "body": "{\"method\":\"{{request.method}}\",\"path\":\"{{request.path}}\",\"q\":\"{{request.query.name}}\",\"h\":\"{{request.header 'X-Trace'}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:20061/greet?name=Ada")
        .header("X-Trace", "abc-123")
        .send()
        .await
        .expect("request");
    let echo_header = resp
        .headers()
        .get("x-echo-method")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/greet");
    assert_eq!(body["q"], "Ada");
    assert_eq!(body["h"], "abc-123");
    assert_eq!(
        echo_header.as_deref(),
        Some("GET"),
        "response headers must be templated too"
    );
    let _ = manager.delete_imposter(20061).await;
}

/// A missing `request.query`/`request.header` value substitutes empty (non-debug policy), not a
/// 500 — the request still succeeds.
#[tokio::test]
async fn templated_true_missing_values_substitute_empty_non_debug() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20062, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "[{{request.query.missing}}][{{request.header 'Nope'}}]" },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:20062/x")
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        200,
        "a missing lookup must not fail the request"
    );
    let body = resp.text().await.expect("body");
    assert_eq!(body, "[][]");
    let _ = manager.delete_imposter(20062).await;
}

/// AC4 script (a): echo `request.body.variant.variantAttribute` into the response —
/// `injection.cjs` reproduced as `{{request.json '$.variant.variantAttribute'}}`.
#[tokio::test]
async fn ac4_variant_attribute_echo() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20063, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "method": "POST", "path": "/orders" } }],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        // A request-derived value placed inside a JSON string literal is escaped
                        // with `| json` (issue #359 B3) so a value containing `"`/`\` can't break
                        // the JSON body.
                        "body": "{\"variantAttribute\":\"{{request.json '$.variant.variantAttribute' | json}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:20063/orders")
        .json(&serde_json::json!({ "variant": { "variantAttribute": "midnight-blue" } }))
        .send()
        .await
        .expect("request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["variantAttribute"], "midnight-blue");
    let _ = manager.delete_imposter(20063).await;
}

/// AC4 script (a) continued: jsonpath array indexing (`$.items[0].id`) and a missing path
/// (non-debug: empty substitution).
#[tokio::test]
async fn request_json_array_index_and_missing_path() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20064, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "method": "POST", "path": "/items" } }],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{\"first\":\"{{request.json '$.items[0].id'}}\",\"missing\":\"[{{request.json '$.nope'}}]\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:20064/items")
        .json(&serde_json::json!({ "items": [{ "id": 42 }, { "id": 43 }] }))
        .send()
        .await
        .expect("request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["first"], "42");
    assert_eq!(body["missing"], "[]");
    let _ = manager.delete_imposter(20064).await;
}

/// AC4 script (b): copy the trailing UUID path segment into `executionId` —
/// `appendExecutionId.cjs` reproduced as `{{request.path | last_segment}}`.
#[tokio::test]
async fn ac4_execution_id_from_trailing_path_segment() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20065, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{\"executionId\":\"{{request.path | last_segment}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp =
        reqwest::get("http://127.0.0.1:20065/executions/11111111-2222-3333-4444-555555555555")
            .await
            .expect("request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["executionId"], "11111111-2222-3333-4444-555555555555",
        "last_segment must extract the trailing path segment"
    );
    let _ = manager.delete_imposter(20065).await;
}

/// `| regex '<pattern>' <group>` extracts a specific capture group from the path.
#[tokio::test]
async fn regex_filter_extracts_capture_group() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20066, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{{request.path | regex '^/orders/(\\d+)/items$' 1}}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let body = reqwest::get("http://127.0.0.1:20066/orders/4711/items")
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "4711");
    let _ = manager.delete_imposter(20066).await;
}

/// AC4 script (c): set a timestamp to `now - 36h` — `timeShift.js` reproduced as
/// `{{now offset='-36h'}}`. Also covers `+2d` and a custom `format`.
#[tokio::test]
async fn ac4_now_with_offset_and_format() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20067, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{\"past\":\"{{now offset='-36h'}}\",\"future\":\"{{now offset='+2d'}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:20067/x")
        .await
        .expect("request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    let past = chrono::DateTime::parse_from_rfc3339(body["past"].as_str().expect("past str"))
        .expect("valid RFC3339");
    let future = chrono::DateTime::parse_from_rfc3339(body["future"].as_str().expect("future str"))
        .expect("valid RFC3339");
    let expected_past = chrono::Utc::now() - chrono::Duration::hours(36);
    let expected_future = chrono::Utc::now() + chrono::Duration::days(2);
    assert!((past.timestamp() - expected_past.timestamp()).abs() <= 5);
    assert!((future.timestamp() - expected_future.timestamp()).abs() <= 5);
    let _ = manager.delete_imposter(20067).await;
}

/// `uuid` renders a valid v4 UUID; `randomInt a b` renders an integer within `[a, b]`.
#[tokio::test]
async fn uuid_and_random_int() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20068, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{\"id\":\"{{uuid}}\",\"n\":\"{{randomInt 10 20}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::get("http://127.0.0.1:20068/x")
        .await
        .expect("request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    let id = body["id"].as_str().expect("id str");
    let parsed = uuid::Uuid::parse_str(id).expect("valid uuid");
    assert_eq!(parsed.get_version_num(), 4);
    let n: i64 = body["n"].as_str().expect("n str").parse().expect("integer");
    assert!((10..=20).contains(&n));
    let _ = manager.delete_imposter(20068).await;
}

/// Issue #359 B1 (template injection — REPRODUCED): reflected request data must NEVER be scanned
/// for `{{ }}` tokens. A stub echoes the raw request body via `${request.body}` with
/// `templated: true`; the POSTed body contains `{{state.secret}}` and `{{uuid}}`. Because the
/// `{{ }}` render runs on the config-authored body BEFORE `${request.*}` injects reflected data,
/// those tokens must come back LITERALLY — the seeded flow-state secret must not leak, and `{{uuid}}`
/// must not be evaluated into a UUID.
#[tokio::test]
async fn reflected_request_data_is_never_templated() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20072, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory" } },
            "stubs": [
                {
                    "predicates": [{ "equals": { "path": "/seed" } }],
                    "responses": [{
                        "_rift": { "script": {
                            "engine": "rhai",
                            "code": "fn respond(ctx) { ctx.state.set(\"secret\", \"TOPSECRET\"); pass() }"
                        } }
                    }]
                },
                {
                    "predicates": [{ "equals": { "path": "/echo" } }],
                    "responses": [{
                        // Config-authored body is pure `${request.*}` reflection — it has no `{{ }}`
                        // of its own, so the only `{{ }}` that could appear is whatever the caller
                        // sent in the request body.
                        "is": { "statusCode": 200, "body": "${request.body}" },
                        "_rift": { "templated": true }
                    }]
                }
            ]
        }),
    )
    .await;

    let _ = reqwest::get("http://127.0.0.1:20072/seed")
        .await
        .expect("seed request");
    let attack_body = "leak={{state.secret}} id={{uuid}}";
    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:20072/echo")
        .body(attack_body)
        .send()
        .await
        .expect("request");
    let body = resp.text().await.expect("body");
    assert_eq!(
        body, attack_body,
        "reflected request data must be served verbatim — `{{{{ }}}}` in the request body must NOT be evaluated"
    );
    assert!(
        !body.contains("TOPSECRET"),
        "the flow-state secret must never leak via reflected request data"
    );
    let _ = manager.delete_imposter(20072).await;
}

/// Issue #359 B3(a): a substituted value containing a `"` must be escaped with `| json` so the
/// response body stays valid JSON (parse it to prove it).
#[tokio::test]
async fn json_filter_keeps_body_valid_when_value_has_quotes() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20073, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "method": "POST", "path": "/echo" } }],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "{\"note\":\"{{request.json '$.note' | json}}\"}"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:20073/echo")
        .json(&serde_json::json!({ "note": "he said \"hi\"\\bye" }))
        .send()
        .await
        .expect("request");
    // If `| json` failed to escape, this body would be invalid JSON and `.json()` would error.
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("json filter keeps the body valid JSON");
    assert_eq!(body["note"], "he said \"hi\"\\bye");
    let _ = manager.delete_imposter(20073).await;
}

/// Issue #359 B3(b): CR/LF in a templated header value must be stripped (not injected as a second
/// header). A templated `X-Echo` header reflects a request JSON value carrying `\r\nInjected: yes`.
#[tokio::test]
async fn header_injection_via_templated_value_is_stripped() {
    let manager = ImposterManager::new();
    spawn(
        &manager,
        serde_json::json!({
            "port": 20074, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "method": "POST", "path": "/echo" } }],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "headers": { "X-Echo": "{{request.json '$.evil'}}" },
                        "body": "ok"
                    },
                    "_rift": { "templated": true }
                }]
            }]
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .post("http://127.0.0.1:20074/echo")
        .json(&serde_json::json!({ "evil": "safe\r\nInjected: yes" }))
        .send()
        .await
        .expect("request");
    assert!(
        resp.headers().get("injected").is_none(),
        "CRLF in a templated header value must not smuggle a second header"
    );
    let echo = resp
        .headers()
        .get("x-echo")
        .and_then(|v| v.to_str().ok())
        .expect("x-echo header present");
    assert!(
        !echo.contains('\r') && !echo.contains('\n'),
        "control characters must be stripped from the templated header value"
    );
    assert_eq!(echo, "safeInjected: yes");
    let _ = manager.delete_imposter(20074).await;
}

/// `state.<key>` reads flow state (read-only) for the request's resolved flow id: present key
/// renders the value, a missing key substitutes empty (non-debug policy).
#[tokio::test]
async fn state_key_present_and_missing() {
    let manager = ImposterManager::new();
    // Default flow_id_source is the imposter port, so a script response on /seed and the
    // templated `is` response on /read (same imposter) share the same flow id.
    spawn(
        &manager,
        serde_json::json!({
            "port": 20069, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory" } },
            "stubs": [
                {
                    "predicates": [{ "equals": { "path": "/seed" } }],
                    "responses": [{
                        "_rift": { "script": {
                            "engine": "rhai",
                            "code": "fn respond(ctx) { ctx.state.set(\"attempts\", 3); pass() }"
                        } }
                    }]
                },
                {
                    "predicates": [{ "equals": { "path": "/read" } }],
                    "responses": [{
                        "is": { "statusCode": 200, "body": "{\"attempts\":\"{{state.attempts}}\",\"missing\":\"[{{state.nope}}]\"}" },
                        "_rift": { "templated": true }
                    }]
                }
            ]
        }),
    )
    .await;

    let _ = reqwest::get("http://127.0.0.1:20069/seed")
        .await
        .expect("seed request");
    let resp = reqwest::get("http://127.0.0.1:20069/read")
        .await
        .expect("read request");
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["attempts"], "3");
    assert_eq!(body["missing"], "[]");
    let _ = manager.delete_imposter(20069).await;
}
