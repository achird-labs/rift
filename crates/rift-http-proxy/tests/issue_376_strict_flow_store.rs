//! Issue #376 gate (strict ON): with `RIFT_STRICT_FLOW_STORE` enabled, a script flow-store op that
//! hits a backend failure RAISES a native script error instead of returning a fallback value +
//! recording `last_error()`. The raise propagates through `should_inject_bounded` to the existing
//! 500 (`x-rift-script-error`). Covered for both engines (Rhai / JS).
//!
//! The failing backend comes from rift-core's `test-backend` feature: `_rift.flowState.backend =
//! "failing"` installs a store whose ops fail. This needs no Docker/Redis.
//!
//! This whole binary runs strict: the env var is set before any flow-store op executes, so the
//! `strict_flow_store()` OnceLock caches `true` for every test here.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

fn enable_strict() {
    // Set before the first request (and thus the first `strict_flow_store()` read) so the process
    // reads strict mode as ON. Idempotent: every test in this binary sets the same value.
    unsafe { std::env::set_var("RIFT_STRICT_FLOW_STORE", "1") };
}

// A should_inject script that reads flow state then declines to inject. Under a failing backend
// the read is the failure point.
fn script_imposter(port: u16, engine: &str, code: &str) -> ImposterConfig {
    cfg(serde_json::json!({
        "protocol": "http", "port": port,
        "_rift": { "flowState": { "backend": "failing" } },
        "stubs": [{
            "predicates": [{ "equals": { "path": "/go" } }],
            "responses": [{ "_rift": { "script": { "engine": engine, "code": code } } }]
        }]
    }))
}

async fn assert_strict_raises(port: u16, engine: &str, code: &str) {
    enable_strict();
    let manager = ImposterManager::new();
    manager
        .create_imposter(script_imposter(port, engine, code))
        .await
        .expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/go"))
        .await
        .expect("request");
    let status = resp.status();
    let has_err_header = resp.headers().contains_key("x-rift-script-error");
    assert_eq!(
        status, 500,
        "strict mode must fail loud (500) when the {engine} flow-store op fails"
    );
    assert!(
        has_err_header,
        "the 500 must carry x-rift-script-error so the failure is visible ({engine})"
    );
    let _ = manager.delete_imposter(port).await;
}

// AC1: Rhai strict flow-store failure raises → 500.
#[tokio::test]
async fn strict_rhai_flow_store_failure_raises() {
    assert_strict_raises(
        19961,
        "rhai",
        r#"fn should_inject(request, flow_store){ flow_store.get("f","k"); #{ inject: false } }"#,
    )
    .await;
}

// AC3: JS strict flow-store failure raises → 500.
#[tokio::test]
async fn strict_js_flow_store_failure_raises() {
    assert_strict_raises(
        19963,
        "javascript",
        r#"function should_inject(request, flow_store){ flow_store.get("f","k"); return { inject: false }; }"#,
    )
    .await;
}
