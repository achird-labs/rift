//! Issue #1755: with `_rift.scriptEngine.seed`, a serialized request sequence replays byte for byte
//! across recreates and restarts: `_rift.fault` rolls, the template `randomInt` and (with the
//! `wasm` feature) a component's `wasi:random` all draw from the imposter's seeded stream.

#[cfg(feature = "wasm")]
#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::{Value, json};

fn imposter(seed: Option<Value>) -> ImposterConfig {
    let mut engine = json!({});
    if let Some(seed) = seed {
        engine["seed"] = seed;
    }
    #[allow(unused_mut)]
    let mut stubs = vec![
        // Half the requests fail, by the stream's roll.
        json!({ "predicates": [{ "equals": { "path": "/fault" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "ok" },
                    "_rift": { "fault": { "error": { "probability": 0.5, "status": 503, "body": "boom" } } } }] }),
        json!({ "predicates": [{ "equals": { "path": "/template" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "{{ randomInt 1 1000000000 }}" },
                    "_rift": { "templated": true } }] }),
    ];
    #[cfg(feature = "wasm")]
    stubs.push(json!({ "predicates": [{ "equals": { "path": "/wasm" } }],
        "responses": [{ "_rift": { "script": { "engine": "wasm",
                "code": wasm_fixtures::b64(&wasm_fixtures::random_body()) } } }] }));
    serde_json::from_value(json!({
        "protocol": "http",
        "_rift": { "scriptEngine": engine },
        "stubs": stubs,
    }))
    .expect("imposter config")
}

/// What one request answered: status, headers but `date`, body.
type Answer = (u16, Vec<(String, String)>, String);

/// The fixed sequence, cycling the stubs; `/wasm` answers its component's draw in the body.
async fn run(port: u16) -> Vec<Answer> {
    #[cfg(feature = "wasm")]
    let paths = ["/fault", "/template", "/wasm"];
    #[cfg(not(feature = "wasm"))]
    let paths = ["/fault", "/template"];
    let client = reqwest::Client::new();
    let mut out = Vec::new();
    for i in 0..10 {
        let path = paths[i % paths.len()];
        let resp = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("request");
        let status = resp.status().as_u16();
        let mut headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| *name != "date")
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_owned()))
            .collect();
        headers.sort();
        out.push((status, headers, resp.text().await.expect("body")));
    }
    out
}

async fn fresh_run(manager: &ImposterManager, seed: Option<Value>) -> Vec<Answer> {
    let port = manager
        .create_imposter(imposter(seed))
        .await
        .expect("create");
    let out = run(port).await;
    manager.delete_imposter(port).await.expect("delete");
    out
}

#[tokio::test]
async fn a_seeded_sequence_replays_across_recreates_and_restarts() {
    let manager = ImposterManager::new();
    let first = fresh_run(&manager, Some(json!(42))).await;
    // The sequence exercises both readers: some faults fire and some do not.
    let faults = first.iter().filter(|(status, ..)| *status == 503).count();
    assert!((1..5).contains(&faults), "{first:?}");
    for attempt in 0..19 {
        assert_eq!(
            fresh_run(&manager, Some(json!(42))).await,
            first,
            "recreate {attempt}"
        );
    }
    // A restart reseeds in place.
    let port = manager
        .create_imposter(imposter(Some(json!(42))))
        .await
        .expect("create");
    assert_eq!(run(port).await, first);
    manager.restart_runtime_state(port).await.expect("restart");
    assert_eq!(run(port).await, first, "after a restart");
    manager.delete_imposter(port).await.expect("delete");

    assert_ne!(
        fresh_run(&manager, Some(json!(43))).await,
        first,
        "another seed"
    );
}

#[tokio::test]
async fn without_a_seed_runs_differ() {
    let manager = ImposterManager::new();
    let a = fresh_run(&manager, None).await;
    let b = fresh_run(&manager, None).await;
    // Five draws in [1, 1e9] each: equal by chance about never.
    assert_ne!(a, b);
}

// A seed engines before #1755 ignored still decodes, and runs unseeded.
#[tokio::test]
async fn a_seed_that_is_not_a_u64_still_creates_the_imposter() {
    let manager = ImposterManager::new();
    for odd in [json!(-1), json!("42")] {
        let port = manager
            .create_imposter(imposter(Some(odd.clone())))
            .await
            .unwrap_or_else(|e| panic!("{odd}: {e}"));
        let config = manager.get_imposter(port).expect("imposter").config.clone();
        let kept = serde_json::to_value(&config).expect("encodes")["_rift"]["scriptEngine"]["seed"]
            .clone();
        assert_eq!(kept, odd, "kept as written");
        manager.delete_imposter(port).await.expect("delete");
    }
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn a_component_draws_the_same_value_after_a_recreate() {
    use rift_mock_core::scripting::{FaultDecision, ScriptCtxExtras, ScriptRequest, WasmScript};
    // Through the imposter's own handle, as the request path scopes it.
    let manager = ImposterManager::new();
    let draw = || async {
        let port = manager
            .create_imposter(imposter(Some(json!(7))))
            .await
            .expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        let code = wasm_fixtures::b64(&wasm_fixtures::random_u64());
        let request = ScriptRequest {
            method: "GET".into(),
            path: "/wasm".into(),
            headers: Default::default(),
            body: Value::Null,
            query: Default::default(),
            path_params: Default::default(),
            raw_body: None,
            mode: rift_mock_core::imposter::ResponseMode::Text,
        };
        let decision = rift_mock_core::imposter::rng::with_request_rng(
            imposter.request_rng(),
            rift_mock_core::scripting::should_inject_wasm(
                WasmScript {
                    code: &code,
                    export: None,
                    knobs: None,
                    limits: Default::default(),
                    clock: None,
                },
                "t",
                &request,
                std::sync::Arc::new(rift_mock_core::backends::InMemoryFlowStore::new(60)),
                std::time::Duration::from_secs(5),
                &ScriptCtxExtras::default(),
            ),
        )
        .await
        .expect("runs");
        manager.delete_imposter(port).await.expect("delete");
        match decision {
            FaultDecision::Latency { duration_ms, .. } => duration_ms,
            other => panic!("expected delay, got {other:?}"),
        }
    };
    assert_eq!(draw().await, draw().await);
}
