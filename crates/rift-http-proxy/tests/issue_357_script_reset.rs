//! Issue #357: a `_rift.script` whose `respond(ctx)` calls the v2 `reset()` result constructor
//! resets the TCP connection (a real transport error) end-to-end through the imposter serve loop's
//! FaultIo — the same behavior as a top-level `fault` / `_rift.fault.tcp`, not a framed HTTP 502.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

#[tokio::test]
async fn script_reset_resets_the_connection() {
    let manager = Arc::new(ImposterManager::new());
    let cfg = serde_json::from_value(serde_json::json!({
        "port": 19947, "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/boom" } }],
            "responses": [{
                "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { reset() }" } }
            }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(cfg).await.expect("create imposter");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // A script reset() must tear down the socket, so the client sees a transport-level error
    // (reqwest Err), not a normal HTTP response. If reset() were mis-wired to a plain 502 (or the
    // H1-only gate were missing and HTTP/2 swallowed the abort), this would return Ok instead.
    let result = reqwest::get("http://127.0.0.1:19947/boom").await;
    assert!(
        result.is_err(),
        "script reset() must reset the connection (transport error), got: {result:?}"
    );

    manager.delete_all().await;
}
