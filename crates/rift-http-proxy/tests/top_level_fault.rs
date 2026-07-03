//! Issue #309: a Mountebank-style top-level `fault` response resets the TCP connection (a real
//! transport error), not a framed HTTP 502 — end-to-end through the imposter serve loop's FaultIo.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

#[tokio::test]
async fn top_level_connection_reset_fault_resets_the_connection() {
    let manager = Arc::new(ImposterManager::new());
    let cfg = serde_json::from_value(serde_json::json!({
        "port": 19940, "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/f" } }],
            "responses": [{ "fault": "CONNECTION_RESET_BY_PEER" }]
        }]
    }))
    .expect("valid imposter config");
    manager.create_imposter(cfg).await.expect("create imposter");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Before the fix this returned a clean 200/502; now the connection is reset, so the client
    // sees a transport-level error (curl rc!=0 / reqwest Err), matching Mountebank.
    let result = reqwest::get("http://127.0.0.1:19940/f").await;
    assert!(
        result.is_err(),
        "top-level fault must reset the connection (transport error), got: {result:?}"
    );

    manager.delete_all().await;
}
