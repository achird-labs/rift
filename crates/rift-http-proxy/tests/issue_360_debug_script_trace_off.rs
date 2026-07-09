//! Issue #360 Item 3 (debug OFF, the default): the `x-rift-script-trace` header must be absent —
//! and, more importantly, must never be BUILT — when debug mode is off. This file deliberately
//! never sets `RIFT_DEBUG` (a separate binary/process from `issue_360_debug_script_trace.rs`,
//! whose whole point is the opposite): `rift_core::util::rift_debug_env()` caches its first read
//! in a `OnceLock`, so debug-on in one binary can't leak into this one.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

#[tokio::test]
async fn matched_script_response_has_no_trace_header_when_debug_is_off() {
    let manager = ImposterManager::new();
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 20091, "protocol": "http",
        "stubs": [{
            "responses": [{
                "_rift": {
                    "script": {
                        "engine": "rhai",
                        "code": r#"fn respond(ctx) { http(200, "hi") }"#
                    }
                }
            }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get("http://127.0.0.1:20091/x")
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert!(
        !resp.headers().contains_key("x-rift-script-trace"),
        "trace header must be absent when debug mode is off"
    );

    let _ = manager.delete_imposter(20091).await;
}
