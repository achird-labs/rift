//! Issue #376 regression guard (strict OFF = default): with `RIFT_STRICT_FLOW_STORE` unset, a
//! script flow-store op that hits a backend failure keeps the #322 lenient contract — the op
//! returns its fallback value (so the script proceeds) instead of raising. A `should_inject` that
//! reads state under a failing backend then declines to inject still serves 200, not a 500.
//!
//! This binary never sets the env var, so `strict_flow_store()` caches `false`.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

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

async fn assert_lenient_serves_200(port: u16, engine: &str, code: &str) {
    let manager = ImposterManager::new();
    manager
        .create_imposter(script_imposter(port, engine, code))
        .await
        .expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/go"))
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        200,
        "default (lenient) mode must not raise on a {engine} flow-store failure (#322 preserved)"
    );
    let _ = manager.delete_imposter(port).await;
}

// AC4 (Rhai): default lenient — a flow-store failure returns the fallback; script serves 200.
#[tokio::test]
async fn lenient_rhai_flow_store_failure_serves_200() {
    assert_lenient_serves_200(
        19971,
        "rhai",
        r#"fn should_inject(request, flow_store){ flow_store.get("f","k"); #{ inject: false } }"#,
    )
    .await;
}

// AC4 (JS): default lenient.
#[tokio::test]
async fn lenient_js_flow_store_failure_serves_200() {
    assert_lenient_serves_200(
        19973,
        "javascript",
        r#"function should_inject(request, flow_store){ flow_store.get("f","k"); return { inject: false }; }"#,
    )
    .await;
}
