//! Issue #1308: stub analysis reads the same gates the matcher does. The documented scenario
//! example (two `/pay` stubs gated `Started` / `paid`) is not a duplicate and is not shadowed.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn start_admin() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None);
    let running = server.bind().await.expect("admin API binds");
    let admin = format!("http://{}", running.local_addr());
    (reqwest::Client::new(), admin, manager)
}

fn pay_stub(state: &str) -> Value {
    json!({
        "scenarioName": "checkout",
        "requiredScenarioState": state,
        "predicates": [{"equals": {"method": "POST", "path": "/pay"}}],
        "responses": [{"is": {"statusCode": 200}}]
    })
}

fn analysis_warnings(body: &Value) -> Vec<Value> {
    body["_rift"]["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter(|w| {
                    matches!(
                        w["warningType"].as_str(),
                        Some("exact_duplicate" | "potentially_shadowed")
                    )
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_scenario_gated_pair_gets_no_duplicate_or_shadow_warning() {
    let (client, admin, manager) = start_admin().await;
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": 0, "protocol": "http",
            "stubs": [pay_stub("Started"), pay_stub("paid")]}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let port = created["port"].as_u64().expect("assigned port") as u16;
    assert!(analysis_warnings(&created).is_empty(), "{created}");

    let fetched: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert!(analysis_warnings(&fetched).is_empty(), "{fetched}");

    // The same predicates with no gate on the second stub really are a duplicate.
    let ungated_twin: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"port": 0, "protocol": "http", "stubs": [
            {"predicates": pay_stub("x")["predicates"], "responses": [{"is": {"statusCode": 200}}]},
            pay_stub("paid")]}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    assert_eq!(analysis_warnings(&ungated_twin).len(), 1, "{ungated_twin}");
    let _ = manager.delete_imposter(port).await;
    if let Some(p) = ungated_twin["port"].as_u64() {
        let _ = manager.delete_imposter(p as u16).await;
    }
}
