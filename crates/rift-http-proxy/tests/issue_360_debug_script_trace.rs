//! Issue #360 Item 3 (debug ON): a matched request whose stub ran a `_rift.script` carries an
//! `x-rift-script-trace` response header showing the script's decision chain — which hook ran,
//! its rendered decision, duration, and captured `ctx.logger` lines.
//!
//! This needs its own process: `RIFT_DEBUG` is read once into a `OnceLock`
//! (`rift_mock_core::util::rift_debug_env`), so it must be set before the very first script run in
//! this binary — same pattern as `issue_359_response_templating_debug.rs`. The "trace absent
//! when debug is off" half of the AC lives in `issue_360_debug_script_trace_off.rs`, a separate
//! binary that never sets `RIFT_DEBUG`, since within one process the flag can only go one way.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::time::Duration;

fn enable_debug() {
    // SAFETY (env mutation, not memory): set before any request is served, matching the
    // established RIFT_DEBUG env-cached-per-process test pattern in this crate.
    unsafe { std::env::set_var("RIFT_DEBUG", "1") };
}

#[tokio::test]
async fn matched_script_response_carries_trace_header_in_debug_mode() {
    enable_debug();
    let manager = ImposterManager::new();
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 20090, "protocol": "http",
        "stubs": [{
            "responses": [{
                "_rift": {
                    "script": {
                        "engine": "rhai",
                        "code": r#"fn respond(ctx) { ctx.logger.info("debug trace test"); http(200, "hi") }"#
                    }
                }
            }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::get("http://127.0.0.1:20090/x")
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let trace_header = resp
        .headers()
        .get("x-rift-script-trace")
        .expect("x-rift-script-trace header must be present in debug mode")
        .to_str()
        .expect("header is valid utf8")
        .to_string();

    let trace: serde_json::Value =
        serde_json::from_str(&trace_header).expect("trace header is valid JSON");
    let entries = trace.as_array().expect("trace is a JSON array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["hook"], "respond");
    assert!(
        entries[0]["decision"]
            .as_str()
            .unwrap()
            .starts_with("http(200)"),
        "got {:?}",
        entries[0]["decision"]
    );
    assert!(entries[0]["durationMs"].is_number());
    assert_eq!(entries[0]["logs"][0], "debug trace test");

    let _ = manager.delete_imposter(20090).await;
}
