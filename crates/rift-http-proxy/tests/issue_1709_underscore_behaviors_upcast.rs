//! Issue #1709: `_behaviors` runs only when the response sets no `behaviors` (a `null` one
//! included) and no top-level `repeat`, as Mountebank upcasts it (`compatibility.js`
//! `canUpcastBehaviors`). Beside either, Mountebank ignores it whole, its `repeat` included; Rift
//! ran it and ignored `behaviors`.
//!
//! The observable is a `copy` of the request path into the body token `${P}`, which needs no
//! `--allowInjection`. The expected sequences are what Mountebank 2.9.5 served in the #1709 probe.
//! Every imposter takes an engine-assigned port.

use std::sync::Arc;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

async fn admin() -> (String, RunningAdminApi) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin API binds");
    (format!("http://{}", running.local_addr()), running)
}

async fn create(url: &str, first: Value) -> (u16, Value) {
    let imposter = json!({"protocol": "http", "stubs": [{"responses": [
        first, {"is": {"body": "second"}}
    ]}]});
    let reply = reqwest::Client::new()
        .post(format!("{url}/imposters"))
        .json(&imposter)
        .send()
        .await
        .expect("POST");
    let status = reply.status().as_u16();
    let body: Value = reply.json().await.expect("json");
    assert_eq!(status, 201, "{body}");
    let port = body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .expect("assigned port");
    (port, imposter)
}

/// The bodies of three requests to `/x`.
async fn served(port: u16) -> Vec<String> {
    let mut bodies = Vec::new();
    for _ in 0..3 {
        bodies.push(
            reqwest::get(format!("http://127.0.0.1:{port}/x"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body"),
        );
    }
    bodies
}

fn copy_path() -> Value {
    json!([{"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".*"}}])
}

fn is_token() -> Value {
    json!({"statusCode": 200, "body": "${P}"})
}

/// The W025 locations rift-lint reports for `imposter`.
fn w025(imposter: &Value) -> Vec<String> {
    rift_lint::lint_json(
        &imposter.to_string(),
        "fixture.json",
        &rift_lint::LintOptions::default(),
    )
    .issues
    .iter()
    .filter(|issue| issue.code == "W025")
    .map(|issue| issue.location.clone().unwrap_or_default())
    .collect()
}

const IGNORED: &str = "stubs[0].responses[0]._behaviors";

#[tokio::test]
async fn underscore_behaviors_beside_behaviors_is_ignored() {
    let (url, _running) = admin().await;
    let (port, imposter) = create(
        &url,
        json!({"is": is_token(), "_behaviors": {"copy": copy_path()}, "behaviors": []}),
    )
    .await;
    assert_eq!(served(port).await, ["${P}", "second", "${P}"]);
    assert_eq!(w025(&imposter), [IGNORED]);
    let echoed: Value = reqwest::get(format!("{url}/imposters/{port}"))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let first = &echoed["stubs"][0]["responses"][0];
    assert!(
        first.get("_behaviors").is_none(),
        "the ignored block is left out: {first}"
    );
}

#[tokio::test]
async fn underscore_behaviors_beside_a_null_behaviors_is_ignored() {
    let (url, _running) = admin().await;
    let (port, imposter) = create(
        &url,
        json!({"is": is_token(), "_behaviors": {"copy": copy_path()}, "behaviors": null}),
    )
    .await;
    assert_eq!(served(port).await, ["${P}", "second", "${P}"]);
    assert_eq!(w025(&imposter), [IGNORED]);
}

#[tokio::test]
async fn underscore_behaviors_beside_a_top_level_repeat_is_ignored() {
    let (url, _running) = admin().await;
    let (port, imposter) = create(
        &url,
        json!({"is": is_token(), "repeat": 2, "_behaviors": {"copy": copy_path(), "repeat": 5}}),
    )
    .await;
    assert_eq!(served(port).await, ["${P}", "${P}", "second"]);
    assert_eq!(w025(&imposter), [IGNORED]);
    let echoed: Value = reqwest::get(format!("{url}/imposters/{port}"))
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let first = &echoed["stubs"][0]["responses"][0];
    assert_eq!(first["repeat"], 2, "{first}");
    assert!(first.get("_behaviors").is_none(), "{first}");
}

#[tokio::test]
async fn underscore_behaviors_alone_runs_with_its_repeat() {
    let (url, _running) = admin().await;
    let (port, imposter) = create(
        &url,
        json!({"is": is_token(), "_behaviors": {"copy": copy_path(), "repeat": 2}}),
    )
    .await;
    assert_eq!(served(port).await, ["/x", "/x", "second"]);
    assert_eq!(w025(&imposter), Vec::<String>::new());
}

// An ignored `_behaviors.decorate` never runs, so it is not a script the `--allowInjection` gate
// refuses, as Mountebank never sees it either.
#[tokio::test]
async fn an_ignored_underscore_decorate_is_admitted_without_injection() {
    let (url, _running) = admin().await;
    let (port, _) = create(
        &url,
        json!({"is": {"body": "plain"}, "behaviors": [],
               "_behaviors": {"decorate": "function (config) { config.response.body = 'x'; }"}}),
    )
    .await;
    assert_eq!(served(port).await, ["plain", "second", "plain"]);
}
