//! Issue #318: typed BackendUnavailable errors surface as structured 503s (data plane and
//! admin), the ResponseDecorator hook fires on both phases, and no-decorator responses are
//! byte-identical to before.
//!
//! The failing backend comes from rift-mock-core's `test-backend` feature (dev-dependency):
//! `_rift.flowState.backend = "failing"` installs a store whose ops annotate then fail
//! with `BackendUnavailable`.

use parking_lot::Mutex;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::extensions::decorate::{ResponseDecorator, ResponsePhase};
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::sync::Arc;

fn imposter_cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

fn failing_backend_cfg(port: u16) -> ImposterConfig {
    imposter_cfg(serde_json::json!({
        "protocol": "http", "port": port,
        "_rift": {"flowState": {"backend": "failing"}},
        "stubs": [{
            "scenarioName": "order",
            "requiredScenarioState": "Started",
            "predicates": [{"equals": {"path": "/gated"}}],
            "responses": [{"is": {"statusCode": 200, "body": "ok"}}]
        }]
    }))
}

async fn start_admin(admin_port: u16, manager: Arc<ImposterManager>) {
    let server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        manager,
        None,
    );
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

type DecoratorCall = (ResponsePhase, Option<u16>, Vec<(String, String)>);

#[derive(Default)]
struct RecordingDecorator {
    calls: Mutex<Vec<DecoratorCall>>,
}

impl ResponseDecorator for RecordingDecorator {
    fn decorate(
        &self,
        phase: ResponsePhase,
        req_port: Option<u16>,
        annotations: &[(&'static str, String)],
        headers: &mut hyper::HeaderMap,
    ) {
        let owned = annotations
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        self.calls.lock().push((phase, req_port, owned));
        headers.insert("x-test-decorated", "1".parse().expect("header"));
    }
}

// AC2 (admin half): every admin response passes through the decorator with phase Admin.
#[tokio::test]
async fn admin_responses_are_decorated() {
    let recorder = Arc::new(RecordingDecorator::default());
    let manager = Arc::new(
        ImposterManager::new()
            .with_response_decorator(recorder.clone() as Arc<dyn ResponseDecorator>),
    );
    start_admin(12618, manager).await;

    let resp = reqwest::get("http://127.0.0.1:12618/health")
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-test-decorated")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "admin response must be decorated"
    );
    let calls = recorder.calls.lock().clone();
    assert!(
        calls
            .iter()
            .any(|(p, port, _)| *p == ResponsePhase::Admin && port.is_none()),
        "decorator must see phase Admin with no port: {calls:?}"
    );
}

// AC3 (data plane, e2e through the manager): a failing backend on a scenario-gated
// request is a structured 503 — never a silent wrong match.
#[tokio::test]
async fn data_plane_scenario_gate_returns_structured_503() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(failing_backend_cfg(19493))
        .await
        .expect("create");

    let resp = reqwest::get("http://127.0.0.1:19493/gated")
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "backendUnavailable");
    assert_eq!(body["feature"], "flowState");

    manager.delete_all().await;
}

// AC3 (admin half): a failing backend behind an admin flow-state endpoint is the same
// structured 503, not an opaque 500.
#[tokio::test]
async fn flow_state_admin_endpoint_returns_structured_503() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(failing_backend_cfg(19494))
        .await
        .expect("create");
    start_admin(12620, manager.clone()).await;

    let resp = reqwest::get("http://127.0.0.1:12620/admin/imposters/19494/flow-state/f/k")
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        503,
        "backend failure must be a structured 503"
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "backendUnavailable");

    manager.delete_all().await;
}

// AC3 (admin half): the scenario inspection endpoint maps the store error too.
#[tokio::test]
async fn scenario_list_maps_backend_error_to_503() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(failing_backend_cfg(19495))
        .await
        .expect("create");
    start_admin(12621, manager.clone()).await;

    let resp = reqwest::get("http://127.0.0.1:12621/imposters/19495/scenarios")
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "backendUnavailable");

    manager.delete_all().await;
}

// AC4: with no decorator and infallible built-ins, the response is byte-identical to
// before this change — pinned as the exact ordered header-name list captured from the
// pre-#318 binary for this stub.
#[tokio::test]
async fn no_decorator_response_headers_unchanged() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(imposter_cfg(serde_json::json!({
            "protocol": "http", "port": 19496,
            "stubs": [{
                "predicates": [{"equals": {"path": "/ping"}}],
                "responses": [{"is": {
                    "statusCode": 200, "body": "pong",
                    "headers": {"content-type": "text/plain"}
                }}]
            }]
        })))
        .await
        .expect("create");

    let resp = reqwest::get("http://127.0.0.1:19496/ping")
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let names: Vec<&str> = resp.headers().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        vec!["content-type", "x-rift-imposter", "content-length", "date"],
        "no-decorator header set must be byte-identical to the pre-#318 baseline"
    );
    assert_eq!(resp.text().await.expect("body"), "pong");

    manager.delete_all().await;
}

// The transition-WRITE path end-to-end: no gate read (no requiredScenarioState), so the
// match succeeds and the 503 comes from the failed newScenarioState write — the "lost
// FSM transition" the issue exists to prevent.
#[tokio::test]
async fn transition_write_failure_returns_structured_503() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(imposter_cfg(serde_json::json!({
            "protocol": "http", "port": 19497,
            "_rift": {"flowState": {"backend": "failing"}},
            "stubs": [{
                "scenarioName": "order",
                "newScenarioState": "paid",
                "predicates": [{"equals": {"path": "/transition"}}],
                "responses": [{"is": {"statusCode": 200, "body": "ok"}}]
            }]
        })))
        .await
        .expect("create");

    let resp = reqwest::get("http://127.0.0.1:19497/transition")
        .await
        .expect("request");
    assert_eq!(resp.status(), 503, "a lost transition must fail loudly");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "backendUnavailable");

    manager.delete_all().await;
}

// Annotations produced by a backend during an ADMIN request reach the Admin decorator —
// proving the with_annotation_scope wiring in server.rs, not just the decorator call.
#[tokio::test]
async fn admin_decorator_receives_backend_annotations() {
    let recorder = Arc::new(RecordingDecorator::default());
    let manager = Arc::new(
        ImposterManager::new()
            .with_response_decorator(recorder.clone() as Arc<dyn ResponseDecorator>),
    );
    manager
        .create_imposter(failing_backend_cfg(19498))
        .await
        .expect("create");
    start_admin(12623, manager.clone()).await;

    let resp = reqwest::get("http://127.0.0.1:12623/admin/imposters/19498/flow-state/f/k")
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    let calls = recorder.calls.lock().clone();
    let admin_call = calls
        .iter()
        .find(|(p, _, _)| *p == ResponsePhase::Admin)
        .expect("admin call recorded");
    assert!(admin_call.1.is_none());
    assert!(
        admin_call.2.iter().any(|(k, _)| k == "flowStore.get"),
        "backend annotation must reach the admin decorator: {calls:?}"
    );

    manager.delete_all().await;
}

// The /__rift/ gateway rides the admin listener: decorated with phase Admin (documented
// design decision), and a failing backend through it is the same structured 503.
#[tokio::test]
async fn gateway_503_is_decorated_with_admin_phase() {
    let recorder = Arc::new(RecordingDecorator::default());
    let manager = Arc::new(
        ImposterManager::new()
            .with_response_decorator(recorder.clone() as Arc<dyn ResponseDecorator>),
    );
    manager
        .create_imposter(failing_backend_cfg(19499))
        .await
        .expect("create");
    start_admin(12624, manager.clone()).await;

    let resp = reqwest::get("http://127.0.0.1:12624/__rift/19499/gated")
        .await
        .expect("request");
    assert_eq!(resp.status(), 503);
    assert_eq!(
        resp.headers()
            .get("x-test-decorated")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "gateway responses are decorated"
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "backendUnavailable");

    let calls = recorder.calls.lock().clone();
    let call = calls
        .iter()
        .find(|(_, _, notes)| notes.iter().any(|(k, _)| k == "flowStore.get"))
        .expect("gateway call recorded with the backend annotation");
    assert_eq!(
        call.0,
        ResponsePhase::Admin,
        "gateway rides the admin phase"
    );
    assert!(call.1.is_none());

    manager.delete_all().await;
}

// The --apikey 401 is produced inside the scope and decorated like any other admin
// response (the decorator sees it; nothing internal leaks since no route ran).
#[tokio::test]
async fn unauthorized_response_is_decorated() {
    let recorder = Arc::new(RecordingDecorator::default());
    let manager = Arc::new(
        ImposterManager::new()
            .with_response_decorator(recorder.clone() as Arc<dyn ResponseDecorator>),
    );
    let server = AdminApiServer::new(
        "127.0.0.1:12625".parse().expect("addr"),
        manager,
        Some("secret-token".to_string()),
    );
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let resp = reqwest::get("http://127.0.0.1:12625/health")
        .await
        .expect("request");
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()
            .get("x-test-decorated")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "401s are decorated too"
    );
    let calls = recorder.calls.lock().clone();
    assert!(
        calls
            .iter()
            .any(|(p, port, notes)| *p == ResponsePhase::Admin
                && port.is_none()
                && notes.is_empty()),
        "auth reject collects no annotations: {calls:?}"
    );
}

// The write-side admin endpoints (PUT/DELETE flow-state, PUT scenario state, POST reset)
// all map a failing backend to the structured 503 — they switched from opaque 500s.
#[tokio::test]
async fn flow_state_write_endpoints_map_backend_error_to_503() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(failing_backend_cfg(19504))
        .await
        .expect("create");
    start_admin(12622, manager.clone()).await;
    let client = reqwest::Client::new();

    let put = client
        .put("http://127.0.0.1:12622/admin/imposters/19504/flow-state/f/k")
        .json(&serde_json::json!({"value": 1}))
        .send()
        .await
        .expect("put");
    assert_eq!(put.status(), 503, "PUT flow-state");

    let del = client
        .delete("http://127.0.0.1:12622/admin/imposters/19504/flow-state/f/k")
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status(), 503, "DELETE flow-state");

    let set_state = client
        .put("http://127.0.0.1:12622/imposters/19504/scenarios/order/state")
        .json(&serde_json::json!({"state": "paid"}))
        .send()
        .await
        .expect("set state");
    assert_eq!(set_state.status(), 503, "PUT scenario state");

    let reset = client
        .post("http://127.0.0.1:12622/imposters/19504/scenarios/reset")
        .send()
        .await
        .expect("reset");
    assert_eq!(reset.status(), 503, "POST scenarios reset");

    manager.delete_all().await;
}
