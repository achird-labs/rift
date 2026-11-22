//! Issue #1794: `GET /imposters/:port/wasm/snapshot` answers a kept wasm instance's
//! `rift:mock/introspect` state, and each reason it cannot with its own status.
#![cfg(feature = "wasm")]

#[path = "../../rift-mock-core/test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use serde_json::{Value, json};

struct Admin {
    base: String,
    client: reqwest::Client,
}

impl Admin {
    async fn create(&self, stubs: Value) -> u64 {
        let created: Value = self
            .client
            .post(format!("{}/imposters", self.base))
            .json(&json!({ "protocol": "http", "stubs": stubs }))
            .send()
            .await
            .expect("create")
            .json()
            .await
            .expect("json");
        created["port"].as_u64().expect("port")
    }

    async fn call(&self, port: u64, path: &str) {
        self.client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("call");
    }

    async fn snapshot(&self, port: u64, query: &str) -> (u16, Option<String>, String) {
        let resp = self
            .client
            .get(format!(
                "{}/imposters/{port}/wasm/snapshot{query}",
                self.base
            ))
            .send()
            .await
            .expect("snapshot");
        let status = resp.status().as_u16();
        let fuel = resp
            .headers()
            .get("x-rift-fuel-used")
            .map(|v| v.to_str().expect("text").to_owned());
        (status, fuel, resp.text().await.expect("body"))
    }
}

fn twin(snapshot: &str, instance: Option<&str>, knobs: Value, path: Option<&str>) -> Value {
    let mut script = json!({
        "engine": "wasm",
        "code": wasm_fixtures::b64(&wasm_fixtures::introspect_counter(snapshot)),
        "knobs": knobs,
    });
    if let Some(instance) = instance {
        script["instance"] = json!(instance);
    }
    let mut stub = json!({ "responses": [{ "_rift": { "script": script } }] });
    if let Some(path) = path {
        stub["predicates"] = json!([{ "startsWith": { "path": path } }]);
    }
    stub
}

#[tokio::test]
async fn the_route_answers_the_snapshot_and_names_each_refusal() {
    let cli = Cli::try_parse_from(["rift", "--host", "127.0.0.1", "--port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli).start().await.expect("server");
    let admin = Admin {
        base: format!("http://{}", server.admin_addr()),
        client: reqwest::Client::new(),
    };

    // 200: the guest's text byte for byte, with the fuel it spent.
    let port = admin
        .create(json!([twin("count", Some("imposter"), json!({}), None)]))
        .await;
    let (status, _, body) = admin.snapshot(port, "").await;
    assert_eq!(status, 409, "not instantiated before a call: {body}");
    assert!(body.contains("not instantiated"), "{body}");
    for _ in 0..3 {
        admin.call(port, "/c").await;
    }
    let (status, fuel, body) = admin.snapshot(port, "").await;
    assert_eq!((status, body.as_str()), (200, r#"{"count":"03"}"#));
    assert!(fuel.is_some_and(|f| f.parse::<u64>().is_ok_and(|f| f > 0)));
    let (status, _, body) = admin.snapshot(port, "?instance=nope").await;
    assert_eq!(status, 409, "an unknown id: {body}");

    // 400: two live instances and no id, listing them.
    let two = admin
        .create(json!([
            twin("count", Some("imposter"), json!({ "n": 1 }), Some("/one")),
            twin("count", Some("imposter"), json!({ "n": 2 }), Some("/two")),
        ]))
        .await;
    admin.call(two, "/one").await;
    admin.call(two, "/two").await;
    let listed: Value = admin
        .client
        .get(format!("{}/imposters/{two}/wasm/instances", admin.base))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let id = listed["instances"][0]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let (status, _, body) = admin.snapshot(two, "").await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(&id), "the ids are listed: {body}");
    let (status, _, body) = admin.snapshot(two, &format!("?instance={id}")).await;
    assert_eq!((status, body.as_str()), (200, r#"{"count":"01"}"#));

    // 409: only `instance: "request"` stubs, pointing at the flow-state route.
    let request = admin
        .create(json!([twin("count", None, json!({}), None)]))
        .await;
    admin.call(request, "/c").await;
    let (status, _, body) = admin.snapshot(request, "").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("flow-state"), "{body}");

    // 404: no introspect export; 500: the guest's err, text that is not JSON, a trap.
    let no_export = admin
        .create(json!([{ "responses": [{ "_rift": { "script": {
            "engine": "wasm",
            "code": wasm_fixtures::b64(&wasm_fixtures::memory_counter("admin-no-introspect")),
            "instance": "imposter",
        } } }] }]))
        .await;
    admin.call(no_export, "/c").await;
    let (status, _, body) = admin.snapshot(no_export, "").await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("rift:mock/introspect"), "{body}");
    for (kind, says) in [("err", "nope"), ("not-json", "not JSON"), ("trap", "")] {
        let port = admin
            .create(json!([twin(kind, Some("imposter"), json!({}), None)]))
            .await;
        admin.call(port, "/c").await;
        let (status, _, body) = admin.snapshot(port, "").await;
        assert_eq!(status, 500, "{kind}: {body}");
        assert!(body.contains(says), "{kind}: {body}");
    }

    // 404: no such imposter.
    let (status, _, _) = admin.snapshot(1, "").await;
    assert_eq!(status, 404);
    server.shutdown().await;
}
