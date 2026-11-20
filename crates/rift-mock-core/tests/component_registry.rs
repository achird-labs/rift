//! Issue #1757: components uploaded to the manager's registry and referenced as
//! `component: "sha256:…"`, and `--strict-wasm` (the manager's trusted list) at every write the
//! manager takes.

#[cfg(feature = "wasm")]
#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use base64::Engine as _;
use rift_mock_core::imposter::components::{ComponentId, TrustedComponents};
use rift_mock_core::imposter::{ImposterConfig, ImposterManager, Stub};
use serde_json::{Value, json};
use std::sync::Arc;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn script_stub(script: Value) -> Value {
    json!({ "responses": [{ "_rift": { "script": script } }] })
}

fn imposter(stubs: Vec<Value>) -> ImposterConfig {
    serde_json::from_value(json!({ "protocol": "http", "stubs": stubs })).expect("config")
}

fn stub(v: Value) -> Stub {
    serde_json::from_value(v).expect("stub")
}

async fn get(port: u16) -> (u16, String) {
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .expect("request");
    (resp.status().as_u16(), resp.text().await.expect("body"))
}

/// A manager trusting only `listed`'s hash. The bytes need not be a real component: strict refusal
/// comes before anything compiles them, and admission at the manager does not compile.
fn strict(listed: &[u8]) -> ImposterManager {
    let trusted = TrustedComponents::parse(&ComponentId::of(listed).hex()).expect("list");
    ImposterManager::new().with_trusted_components(Arc::new(trusted))
}

#[tokio::test]
async fn strict_wasm_refuses_an_unlisted_component_at_every_write() {
    let manager = strict(b"listed");
    let listed = script_stub(json!({ "engine": "wasm", "code": b64(b"listed") }));
    let unlisted = script_stub(json!({ "engine": "wasm", "code": b64(b"other") }));
    let unlisted_ref = script_stub(json!({ "component": ComponentId::of(b"other").to_string() }));
    let refused = |e: rift_mock_core::imposter::ImposterError| {
        let msg = e.to_string();
        assert!(msg.contains("is not in --strict-wasm"), "{msg}");
    };

    // Create.
    refused(
        manager
            .create_imposter(imposter(vec![unlisted.clone()]))
            .await
            .expect_err("create"),
    );
    refused(
        manager
            .create_imposter(imposter(vec![unlisted_ref]))
            .await
            .expect_err("by reference"),
    );
    let port = manager
        .create_imposter(imposter(vec![listed.clone()]))
        .await
        .expect("a listed component is admitted");

    // Every stub edit.
    refused(
        manager
            .add_stub(port, stub(unlisted.clone()), None)
            .await
            .expect_err("add"),
    );
    refused(
        manager
            .replace_stubs(port, vec![stub(unlisted.clone())])
            .await
            .expect_err("replace all"),
    );
    refused(
        manager
            .replace_stub(port, 0, stub(unlisted.clone()))
            .await
            .expect_err("replace one"),
    );
    let mut with_id = unlisted.clone();
    with_id["id"] = json!("s");
    let mut listed_id = listed.clone();
    listed_id["id"] = json!("s");
    manager
        .replace_stubs(port, vec![stub(listed_id)])
        .await
        .expect("listed");
    refused(
        manager
            .replace_stub_by_id(port, "s", stub(with_id))
            .await
            .expect_err("by id"),
    );
    let responses = stub(unlisted.clone()).responses;
    refused(
        manager
            .append_stub_responses_by_id(port, "s", responses)
            .await
            .expect_err("append"),
    );

    // Apply, as an in-place patch (only the stubs change) and as a replace (more changes): either
    // way refused, and the running imposter is left as it was.
    for desired in [
        json!({ "protocol": "http", "port": port, "stubs": [unlisted.clone()] }),
        json!({ "protocol": "http", "port": port, "name": "renamed", "recordRequests": true,
                "stubs": [unlisted.clone()] }),
    ] {
        let report = manager
            .apply_config(vec![serde_json::from_value(desired).expect("config")])
            .await
            .expect("apply");
        assert_eq!(report.failed.len(), 1, "{report:?}");
        refused(report.failed.into_iter().next().expect("failed").1);
        assert!(
            manager.get_imposter(port).is_ok(),
            "the imposter survives a refused apply"
        );
    }

    // Nothing unlisted got in: the imposter still serves its one listed stub.
    let held = manager.get_imposter(port).expect("imposter");
    assert_eq!(held.get_stubs().len(), 1);
    manager.delete_imposter(port).await.expect("delete");

    // An upload is refused before it is compiled.
    refused(manager.upload_component(b"other").expect_err("upload"));
}

#[tokio::test]
async fn without_strict_wasm_anything_is_admitted() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(imposter(vec![script_stub(
            json!({ "engine": "wasm", "code": b64(b"anything") }),
        )]))
        .await
        .expect("create");
    manager.delete_imposter(port).await.expect("delete");
}

// A reference this node does not hold loads, and the stub answers a script error.
#[tokio::test]
async fn a_dangling_reference_loads_and_answers_a_script_error() {
    let manager = ImposterManager::new();
    let missing = ComponentId::of(b"not uploaded");
    let port = manager
        .create_imposter(imposter(vec![script_stub(
            json!({ "component": missing.to_string() }),
        )]))
        .await
        .expect("a dangling reference still loads");
    let (status, body) = get(port).await;
    assert_eq!(status, 500);
    assert!(
        body.contains(&format!("unknown component {missing}")),
        "{body}"
    );
    manager.delete_imposter(port).await.expect("delete");
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn an_uploaded_component_serves_by_reference_and_reports_its_references() {
    let manager = ImposterManager::new();
    let bytes = wasm_fixtures::respond_ok();
    let (entry, new) = manager.upload_component(&bytes).expect("upload");
    assert!(new);
    assert_eq!(entry.id, ComponentId::of(&bytes));
    assert_eq!(entry.size, bytes.len());
    assert!(
        entry
            .imports
            .iter()
            .any(|i| i.starts_with("rift:mock/types@")),
        "{:?}",
        entry.imports
    );
    let (again, new) = manager.upload_component(&bytes).expect("same bytes");
    assert!(!new, "the same bytes again are not new");
    assert!(Arc::ptr_eq(&entry, &again));

    let port = manager
        .create_imposter(imposter(vec![script_stub(
            json!({ "component": entry.id.to_string() }),
        )]))
        .await
        .expect("create");
    assert_eq!(get(port).await, (200, "ok".into()));
    assert_eq!(manager.component_references(&entry.id), vec![port]);
    manager.delete_imposter(port).await.expect("delete");
    assert!(manager.component_references(&entry.id).is_empty());
}

// A door that does not validate (the C-ABI create, a replay) can carry `component` with another
// engine: its bytes never run as Rhai or JavaScript.
#[cfg(feature = "wasm")]
#[tokio::test]
async fn a_component_under_another_engine_answers_a_script_error() {
    let manager = ImposterManager::new();
    let (entry, _) = manager
        .upload_component(&wasm_fixtures::respond_ok())
        .expect("upload");
    let port = manager
        .create_imposter(imposter(vec![script_stub(
            json!({ "engine": "rhai", "component": entry.id.to_string() }),
        )]))
        .await
        .expect("create");
    let (status, body) = get(port).await;
    assert_eq!(status, 500);
    assert!(body.contains("engine must be"), "{body}");
    manager.delete_imposter(port).await.expect("delete");
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn an_upload_is_validated_as_admission_is() {
    let manager = ImposterManager::new();
    let err = manager
        .upload_component(&wasm_fixtures::core_module())
        .expect_err("a core module is not a component");
    assert!(err.to_string().contains("not a component"), "{err}");
    let err = manager
        .upload_component(&wasm_fixtures::importing("sockets"))
        .expect_err("a socket import is refused");
    assert!(err.to_string().contains("wasi:sockets"), "{err}");
    assert!(manager.components().list().is_empty());
}
