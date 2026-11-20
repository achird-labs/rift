//! Issue #1751: the `examples/wasm/counter-twin` component, built by a real wasm32-wasip2
//! toolchain, behind an imposter: it answers 1, 2, then 409 with `knobs.limit: 2`, and its stdout
//! line reaches the `x-rift-script-trace` logs under `RIFT_DEBUG`.
//!
//! Ignored by default: it needs the component built first. The CI `wasm` job builds it and runs
//! this test with `--ignored`:
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 --manifest-path examples/wasm/counter-twin/Cargo.toml
//! cargo test -p rift-http-proxy --test wasm_counter_twin -- --ignored
//! ```
//!
//! Its own binary: `RIFT_DEBUG` is read once per process (`rift_debug_env`).
#![cfg(feature = "wasm")]

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager, ScriptBaseDir, resolve_scripts};
use serde_json::{Value, json};
use std::path::PathBuf;

fn component_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/wasm/counter-twin/target/wasm32-wasip2/release/counter_twin.wasm")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs examples/wasm/counter-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn counter_twin_counts_to_its_limit_and_logs_each_request() {
    // SAFETY (env mutation, not memory): set before any request is served, as the other
    // RIFT_DEBUG tests in this crate do.
    unsafe { std::env::set_var("RIFT_DEBUG", "1") };
    let path = component_path();
    assert!(
        path.exists(),
        "build the example first: {} is missing",
        path.display()
    );
    // The example's own imposter, minus its fixed port, resolved as --configfile resolves it.
    let example_dir = path.ancestors().nth(4).expect("example dir").to_path_buf();
    let text = std::fs::read_to_string(example_dir.join("imposter.json")).expect("imposter.json");
    let mut doc: Value = serde_json::from_str(&text).expect("json");
    doc.as_object_mut().expect("object").remove("port");
    let mut config: ImposterConfig = serde_json::from_value(doc).expect("config");
    resolve_scripts(&mut config, &ScriptBaseDir::ConfigRelative(example_dir)).expect("resolve");

    let manager = ImposterManager::new();
    let port = manager.create_imposter(config).await.expect("create");
    let mut answers = Vec::new();
    let mut logs = Vec::new();
    for _ in 0..3 {
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/count"))
            .await
            .expect("request");
        let status = resp.status().as_u16();
        let trace: Value = serde_json::from_str(
            resp.headers()["x-rift-script-trace"]
                .to_str()
                .expect("ascii trace"),
        )
        .expect("trace json");
        logs.push(trace[0]["logs"].clone());
        answers.push((status, resp.text().await.expect("body")));
    }
    assert_eq!(
        answers,
        [
            (200, r#"{"count":1}"#.to_string()),
            (200, r#"{"count":2}"#.to_string()),
            (
                409,
                r#"{"error":"above limit","count":3,"limit":2}"#.to_string()
            ),
        ]
    );
    assert_eq!(
        logs,
        [
            json!(["count is now 1"]),
            json!(["count is now 2"]),
            json!(["count is now 3"])
        ]
    );
}
