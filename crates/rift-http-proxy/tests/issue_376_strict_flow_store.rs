//! A script `ctx.state` op that hits a backend failure RAISES a native script error instead of
//! returning a fallback value. The raise propagates through `should_inject_bounded` to the
//! existing 500 (`x-rift-script-error`). Covered for both engines (Rhai / JS).
//!
//! The failing store is a test `FlowStoreProvider` (`support/failing_flow_store.rs`) injected
//! through `ImposterManager::with_flow_store_provider`. This needs no Docker.
//!
//! `ctx.state` is unconditionally fail-loud (issue #358).

use rift_http_proxy::imposter::ImposterConfig;
use std::time::Duration;

#[path = "support/failing_flow_store.rs"]
mod failing_flow_store;
use failing_flow_store::failing_manager;

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

// A respond(ctx) script that reads ctx.state then passes through. Under a failing backend the
// read is the failure point.
fn script_imposter(engine: &str, code: &str) -> ImposterConfig {
    cfg(serde_json::json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/go" } }],
            "responses": [{ "_rift": { "script": { "engine": engine, "code": code } } }]
        }]
    }))
}

async fn assert_fails_loud(engine: &str, code: &str) {
    let manager = failing_manager();
    let port = manager
        .create_imposter(script_imposter(engine, code))
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
        "ctx.state must fail loud (500) when the {engine} flow-store op fails"
    );
    assert!(
        has_err_header,
        "the 500 must carry x-rift-script-error so the failure is visible ({engine})"
    );
    let _ = manager.delete_imposter(port).await;
}

// AC1: Rhai ctx.state failure raises → 500.
#[tokio::test]
async fn rhai_flow_store_failure_raises() {
    assert_fails_loud("rhai", r#"fn respond(ctx){ ctx.state.get("k"); pass() }"#).await;
}

// AC3: JS ctx.state failure raises → 500.
#[tokio::test]
async fn js_flow_store_failure_raises() {
    assert_fails_loud(
        "javascript",
        r#"function respond(ctx){ ctx.state.get("k"); return pass(); }"#,
    )
    .await;
}
