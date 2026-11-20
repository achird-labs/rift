//! The published `rift:mock` contract (issue #1797): the WIT ships inside this crate, and Rift passes
//! its own conformance script.
//!
//! `the_conformance_script_passes_on_the_in_memory_store` needs the conformance guest built for
//! wasm32-wasip2 (`wit/conformance/`), so it is ignored by default; CI's wasm example job builds it
//! and runs it with `--ignored`. `RIFT_CONFORMANCE_COMPONENT` points at another build.

use std::path::{Path, PathBuf};

fn wit_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("wit")
}

fn package_line() -> String {
    let wit =
        std::fs::read_to_string(wit_dir().join("rift-mock.wit")).expect("the WIT is in the crate");
    wit.lines()
        .find_map(|l| l.trim().strip_prefix("package "))
        .map(|p| p.trim_end_matches(';').to_owned())
        .expect("a package line")
}

fn script() -> serde_json::Value {
    let text = std::fs::read_to_string(wit_dir().join("conformance/script.json")).expect("script");
    serde_json::from_str(&text).expect("script is JSON")
}

// `bindgen!` reads `wit/` relative to the crate, and `cargo publish` packages only the crate's
// directory: the WIT must stay here, not at the repository root.
#[test]
fn the_wit_is_inside_the_crate_and_the_script_names_its_version() {
    assert_eq!(package_line(), "rift:mock@0.1.0");
    assert_eq!(
        script()["contract"],
        package_line(),
        "script.json's contract"
    );
}

#[cfg(feature = "wasm")]
#[tokio::test]
#[ignore = "needs the conformance guest built for wasm32-wasip2"]
async fn the_conformance_script_passes_on_the_in_memory_store() {
    use base64::Engine as _;
    use rift_mock_core::backends::InMemoryFlowStore;
    use rift_mock_core::imposter::{ResponseMode, WasmLimits};
    use rift_mock_core::scripting::{
        FaultDecision, ScriptCtxExtras, ScriptRequest, WasmScript, should_inject_wasm,
    };
    use std::sync::Arc;

    let path = std::env::var_os("RIFT_CONFORMANCE_COMPONENT").map_or_else(
        || wit_dir().join("conformance/target/wasm32-wasip2/release/rift_mock_conformance.wasm"),
        PathBuf::from,
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let code = base64::engine::general_purpose::STANDARD.encode(bytes);
    let knobs = script();
    let request = ScriptRequest {
        method: "GET".into(),
        path: "/conformance".into(),
        headers: Default::default(),
        body: serde_json::Value::Null,
        query: Default::default(),
        path_params: Default::default(),
        raw_body: None,
        mode: ResponseMode::Text,
    };
    let decision = should_inject_wasm(
        WasmScript {
            code: &code,
            export: None,
            knobs: Some(&knobs),
            limits: WasmLimits::default(),
            clock: None,
        },
        "conformance",
        &request,
        Arc::new(InMemoryFlowStore::new(300)),
        std::time::Duration::from_secs(10),
        &ScriptCtxExtras {
            flow_id: Some("conformance".into()),
            ..Default::default()
        },
    )
    .await
    .expect("the guest runs");
    let FaultDecision::Error { status, body, .. } = decision else {
        panic!("the guest answers a report, got {decision:?}");
    };
    let report: serde_json::Value = serde_json::from_str(&body).expect("report is JSON");
    assert_eq!(status, 200, "{report:#}");
    let steps = knobs["steps"].as_array().map_or(0, Vec::len);
    assert!(steps > 40, "the whole script ran: {steps} steps");
    assert_eq!(report["steps"], serde_json::json!(steps));
}
