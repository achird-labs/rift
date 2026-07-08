//! Issue #356: a YAML configfile using `_rift.script.file:` must resolve to the same effective
//! `ImposterConfig` as the equivalent JSON configfile using inline `code:` — the "06-stateful-retry"
//! sample rewritten to source its script from a file instead of a JSON-escaped one-liner.
//!
//! This is a *behavioral* parity assertion, not string equality of the two source files: the
//! JSON's `_rift.script` carries `code` directly while the YAML's carries `file` (the bookkeeping
//! that names the source differs by design), so the two resolved `RiftScriptConfig`s are compared
//! field-by-field on what actually matters at runtime (`engine`, and the resolved `code` content),
//! and — the real proof of "same effective config" — both scripts are executed through the Rhai
//! engine against an identical request/flow-store sequence and must produce identical decisions.

use rift_http_proxy::backends::InMemoryFlowStore;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::StubResponse;
use rift_http_proxy::scripting::{FaultDecision, RhaiEngine, ScriptRequest};
use std::sync::Arc;

/// The retry-until-success Rhai script: fails (503) the first two attempts per `x-flow-id`, then
/// succeeds. Kept as one constant so the JSON-inline and file-sourced configs carry byte-identical
/// script content — proving the two authoring styles resolve to the same effective config.
const RETRY_SCRIPT: &str = r#"fn should_inject(request, flow_store) { let flow_id = request.headers["x-flow-id"]; if flow_id == () { flow_id = "default"; }; let attempts = flow_store.get(flow_id, "attempts"); if attempts == () { attempts = 0; }; attempts += 1; flow_store.set(flow_id, "attempts", attempts); if attempts <= 2 { #{inject: true, fault: "error", status: 503, body: `{"error":"Temporary failure","attempt":${attempts}}`, headers: #{"Content-Type": "application/json"}} } else { #{inject: false} } }"#;

fn script_config(
    config: &rift_http_proxy::imposter::ImposterConfig,
) -> rift_http_proxy::imposter::RiftScriptConfig {
    match &config.stubs[0].responses[0] {
        StubResponse::RiftScript { rift } => rift.script.clone().expect("script present"),
        other => panic!("expected a RiftScript response, got {other:?}"),
    }
}

fn req(attempt_tag: &str) -> ScriptRequest {
    let mut headers = std::collections::HashMap::new();
    headers.insert("x-flow-id".to_string(), attempt_tag.to_string());
    ScriptRequest {
        method: "GET".to_string(),
        path: "/retry".to_string(),
        headers,
        body: serde_json::Value::Null,
        query: Default::default(),
        path_params: Default::default(),
    }
}

/// (status, body) for an `Error` decision, or `None` for `FaultDecision::None`. `Latency` isn't
/// produced by this script so it's left unmatched (a test failure if it ever were).
fn decision_summary(decision: &FaultDecision) -> Option<(u16, String)> {
    match decision {
        FaultDecision::None => None,
        FaultDecision::Error { status, body, .. } => Some((*status, body.clone())),
        FaultDecision::Latency { .. } => panic!("script never injects latency"),
    }
}

#[test]
fn yaml_file_config_behaves_like_json_inline_config() {
    // JSON original: script inline as `code:` (matches docs/features/scripting.md's "Retry
    // Simulation" example).
    let json_dir = tempfile::tempdir().unwrap();
    let json_config = serde_json::json!({
        "port": 4545,
        "protocol": "http",
        "_rift": {
            "flowState": {"backend": "inmemory", "ttlSeconds": 300}
        },
        "stubs": [{
            "responses": [{
                "_rift": {
                    "script": {
                        "engine": "rhai",
                        "code": RETRY_SCRIPT,
                    }
                }
            }]
        }]
    });
    let json_path = json_dir.path().join("retry.json");
    std::fs::write(&json_path, serde_json::to_string(&json_config).unwrap()).unwrap();

    // YAML rewrite: the same script, but sourced from a companion file (issue #356 `file:`).
    let yaml_dir = tempfile::tempdir().unwrap();
    std::fs::write(yaml_dir.path().join("fail-twice.rhai"), RETRY_SCRIPT).unwrap();
    // `config_loader::load_file` parses a bare (non `{`/`[`-prefixed) file as a YAML sequence of
    // imposters, so the document root is a `- ` list even for a single imposter.
    let yaml_config = "\
- port: 4545
  protocol: http
  _rift:
    flowState:
      backend: inmemory
      ttlSeconds: 300
  stubs:
    - responses:
        - _rift:
            script:
              file: fail-twice.rhai
";
    let yaml_path = yaml_dir.path().join("retry.yaml");
    std::fs::write(&yaml_path, yaml_config).unwrap();

    let json_configs = load_configs(&ConfigSource::File {
        path: json_path,
        no_parse: false,
    })
    .expect("JSON config resolves");
    let yaml_configs = load_configs(&ConfigSource::File {
        path: yaml_path,
        no_parse: false,
    })
    .expect("YAML+file config resolves");

    assert_eq!(json_configs.len(), 1);
    assert_eq!(yaml_configs.len(), 1);

    // Everything outside the script's own source-of-truth bookkeeping matches exactly.
    assert_eq!(json_configs[0].port, yaml_configs[0].port);
    assert_eq!(json_configs[0].protocol, yaml_configs[0].protocol);
    assert_eq!(json_configs[0].stubs.len(), yaml_configs[0].stubs.len());
    assert_eq!(
        json_configs[0].stubs[0].responses.len(),
        yaml_configs[0].stubs[0].responses.len()
    );

    // The resolved script itself: `file:` must have populated `code` identically to the JSON's
    // inline `code:`, and the engine must have been inferred to the same value that was explicit
    // in the JSON.
    let json_script = script_config(&json_configs[0]);
    let yaml_script = script_config(&yaml_configs[0]);
    assert_eq!(
        json_script.code, yaml_script.code,
        "resolved `code` must match"
    );
    assert_eq!(
        json_script.engine.as_deref(),
        Some("rhai"),
        "JSON's explicit engine"
    );
    assert_eq!(
        yaml_script.engine.as_deref(),
        Some("rhai"),
        "YAML's engine must be inferred from the .rhai extension to the same value"
    );

    // Behavioral parity: run both resolved scripts through the Rhai engine against an identical
    // request/flow-store sequence (3 attempts on the same flow_id) and require identical
    // decisions at every step — the actual proof that "sourced via file:" behaves like "inline".
    let json_engine = RhaiEngine::new(&json_script.code.unwrap(), "json").unwrap();
    let yaml_engine = RhaiEngine::new(&yaml_script.code.unwrap(), "yaml").unwrap();
    let json_store: Arc<dyn rift_http_proxy::flow_state::FlowStore> =
        Arc::new(InMemoryFlowStore::new(300));
    let yaml_store: Arc<dyn rift_http_proxy::flow_state::FlowStore> =
        Arc::new(InMemoryFlowStore::new(300));

    for attempt in 1..=3 {
        let request = req("parity-flow");
        let json_decision = json_engine
            .should_inject_fault(&request, json_store.clone())
            .unwrap();
        let yaml_decision = yaml_engine
            .should_inject_fault(&request, yaml_store.clone())
            .unwrap();
        assert_eq!(
            decision_summary(&json_decision),
            decision_summary(&yaml_decision),
            "attempt {attempt}: JSON-inline and YAML+file scripts must decide identically"
        );
    }
}
