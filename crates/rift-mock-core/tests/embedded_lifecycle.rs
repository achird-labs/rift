//! Issue #203 gate: the `rift-mock-core` engine drives the full imposter lifecycle in-process —
//! no admin HTTP server, no `clap` CLI — which is the CLI-free surface the embedded `rift-ffi`
//! backend depends on. If this file compiles and passes, the core is usable standalone.

use rift_mock_core::imposter::ImposterManager;

#[tokio::test]
async fn engine_serves_matches_and_records_without_admin_server_or_cli() {
    // Construct the engine and a stub directly — no AdminApiServer, no arg parsing.
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 19890, "protocol": "http", "recordRequests": true,
        "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300 } },
        "stubs": [
            { "predicates": [{ "equals": { "path": "/hello" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "world" } }] }
        ]
    }))
    .unwrap();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");

    // The mock serves on its own port with nothing but the core in the loop.
    let resp = reqwest::get("http://127.0.0.1:19890/hello")
        .await
        .expect("request the mock");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "world");

    // Recorded-request access and stub management are reachable straight off the manager.
    let imposter = manager.get_imposter(19890).expect("imposter handle");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, "/hello");

    // A second, unmatched path records but does not match the stub (Mountebank no-match → 200).
    let miss = reqwest::get("http://127.0.0.1:19890/nope")
        .await
        .expect("request unmatched");
    assert_eq!(miss.status(), 200);
    assert_ne!(miss.text().await.unwrap(), "world");
    assert_eq!(imposter.get_recorded_requests().len(), 2);

    // Flow-state KV is reachable straight off the core handle — the programmatic surface the
    // embedded (rift-ffi) backend drives instead of the admin HTTP endpoints.
    imposter
        .flow_set("flow", "k", serde_json::json!(42))
        .expect("flow_set");
    assert_eq!(
        imposter.flow_get("flow", "k").expect("flow_get"),
        Some(serde_json::json!(42))
    );
    imposter.flow_delete("flow", "k").expect("flow_delete");
    assert_eq!(imposter.flow_get("flow", "k").expect("flow_get"), None);

    let _ = manager.delete_imposter(19890).await;
}
