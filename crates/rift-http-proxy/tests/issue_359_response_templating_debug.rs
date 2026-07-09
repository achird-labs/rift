//! Issue #359 AC3 (debug ON): with `RIFT_DEBUG` enabled, an unknown/malformed `{{ }}` template
//! function fails the request loudly (500 + `x-rift-template-error`) instead of silently
//! substituting an empty string.
//!
//! This needs its own process: `RIFT_DEBUG` is read once into a `OnceLock` (mirrors
//! `RIFT_STRICT_BEHAVIORS`/`RIFT_STRICT_FLOW_STORE`), so it must be set before the very first
//! template render in this binary.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn enable_debug() {
    // SAFETY (env mutation, not memory): set before any request is served, so every test in this
    // binary reads the same (debug-on) value — matches the existing RIFT_STRICT_FLOW_STORE test
    // pattern (issue_376_strict_flow_store.rs).
    unsafe { std::env::set_var("RIFT_DEBUG", "1") };
}

#[tokio::test]
async fn unknown_function_is_a_request_time_error_in_debug_mode() {
    enable_debug();
    let manager = ImposterManager::new();
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 20070, "protocol": "http",
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "{{bogusFunction}}" },
                "_rift": { "templated": true }
            }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get("http://127.0.0.1:20070/x")
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        500,
        "an unknown template function must fail the request in debug mode"
    );
    assert!(
        resp.headers().contains_key("x-rift-template-error"),
        "the 500 must carry x-rift-template-error so the failure is visible"
    );
    let _ = manager.delete_imposter(20070).await;
}

/// A malformed token (missing required argument) is the same debug-mode error, not a panic.
#[tokio::test]
async fn malformed_token_is_a_request_time_error_in_debug_mode() {
    enable_debug();
    let manager = ImposterManager::new();
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 20071, "protocol": "http",
        "stubs": [{
            "responses": [{
                // request.header requires a quoted header name argument.
                "is": { "statusCode": 200, "body": "{{request.header}}" },
                "_rift": { "templated": true }
            }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get("http://127.0.0.1:20071/x")
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        500,
        "a malformed token must fail loud in debug mode"
    );
    let _ = manager.delete_imposter(20071).await;
}
