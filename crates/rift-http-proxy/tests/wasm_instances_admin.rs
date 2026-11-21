//! Issue #1793: `GET /imposters/:port/wasm/instances` lists an imposter's kept wasm instances.
#![cfg(feature = "wasm")]

#[path = "../../rift-mock-core/test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use serde_json::{Value, json};

#[tokio::test]
async fn the_route_lists_kept_instances() {
    let cli = Cli::try_parse_from(["rift", "--host", "127.0.0.1", "--port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli).start().await.expect("server");
    let admin = format!("http://{}", server.admin_addr());
    let client = reqwest::Client::new();
    let code = wasm_fixtures::b64(&wasm_fixtures::memory_counter("admin"));
    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(
            &json!({ "protocol": "http", "stubs": [{ "responses": [{ "_rift": { "script": {
            "engine": "wasm", "code": code, "instance": "imposter", "knobs": { "k": 1 }
        } } }] }] }),
        )
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("json");
    let port = created["port"].as_u64().expect("port");
    let list = || async {
        client
            .get(format!("{admin}/imposters/{port}/wasm/instances"))
            .send()
            .await
            .expect("list")
            .json::<Value>()
            .await
            .expect("json")
    };
    assert_eq!(
        list().await,
        json!({ "instances": [] }),
        "none until a call"
    );
    for _ in 0..2 {
        client
            .get(format!("http://127.0.0.1:{port}/c"))
            .send()
            .await
            .expect("call");
    }
    let listed = list().await;
    let instance = &listed["instances"][0];
    assert_eq!(
        listed["instances"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    assert_eq!(instance["calls"], 2, "{listed}");
    assert_eq!(
        instance["sha256"].as_str().map(str::len),
        Some(64),
        "{listed}"
    );
    assert_eq!(instance["id"].as_str().map(str::len), Some(64), "{listed}");
    assert_eq!(
        instance["knobsHash"].as_str().map(str::len),
        Some(12),
        "{listed}"
    );
    assert_eq!(instance["export"], Value::Null, "{listed}");
    assert!(instance["instantiatedAt"].as_u64().is_some(), "{listed}");
    assert!(
        instance["memoryBytes"].as_u64().is_some_and(|b| b > 0),
        "{listed}"
    );
    let missing = client
        .get(format!("{admin}/imposters/1/wasm/instances"))
        .send()
        .await
        .expect("missing");
    assert_eq!(missing.status(), 404);
    // An unknown `instance` is refused when the stub is admitted.
    let refused = client
        .post(format!("{admin}/imposters"))
        .json(
            &json!({ "protocol": "http", "stubs": [{ "responses": [{ "_rift": { "script": {
            "engine": "wasm", "code": wasm_fixtures::b64(&wasm_fixtures::memory_counter("x")),
            "instance": "forever"
        } } }] }] }),
        )
        .send()
        .await
        .expect("refused");
    assert_eq!(refused.status(), 400);
    server.shutdown().await;
}
