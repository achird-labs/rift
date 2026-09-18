//! Issue #1159: `_rift.scriptEngine.defaultEngine` was parsed, documented and emitted by the SDKs,
//! and read by nothing — an inline `_rift.script` with no `engine` always ran as Rhai, so
//! `"defaultEngine": "javascript"` handed the author's JavaScript to the Rhai compiler.
//!
//! Both admin doors are covered: whole-imposter create carries the `_rift` block, but a stub added
//! later through `POST /imposters/:port/stubs` arrives without it, so the default has to come from
//! the running imposter.
#![cfg(feature = "javascript")]

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn js_script(body: &str) -> serde_json::Value {
    serde_json::json!({ "_rift": { "script": {
        "code": format!("function respond(ctx) {{ return http(200, '{body}'); }}")
    } } })
}

async fn start_admin() -> (reqwest::Client, String, Arc<ImposterManager>) {
    let manager = Arc::new(ImposterManager::new());
    let admin_port = free_port();
    let server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        manager.clone(),
        None,
    )
    .with_allow_injection(true);
    tokio::spawn(server.run());
    let admin = format!("http://127.0.0.1:{admin_port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{admin}/imposters"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (client, admin, manager)
}

async fn get(client: &reqwest::Client, url: String) -> (u16, String) {
    let response = client.get(url).send().await.expect("GET");
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

#[tokio::test]
async fn an_engine_less_script_runs_in_the_imposters_default_engine() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let created = client
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "port": port,
            "protocol": "http",
            "_rift": { "scriptEngine": { "defaultEngine": "javascript" } },
            "stubs": [{
                "predicates": [{ "equals": { "path": "/created" } }],
                "responses": [js_script("from-create")]
            }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(
        created.status().as_u16(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );

    assert_eq!(
        get(&client, format!("http://127.0.0.1:{port}/created")).await,
        (200, "from-create".to_owned())
    );

    // The stub door: the stub carries no `_rift.scriptEngine` of its own.
    let added = client
        .post(format!("{admin}/imposters/{port}/stubs"))
        .json(&serde_json::json!({ "stub": {
            "predicates": [{ "equals": { "path": "/added" } }],
            "responses": [js_script("from-stub-door")]
        } }))
        .send()
        .await
        .expect("POST stubs");
    assert!(
        added.status().is_success(),
        "{}",
        added.text().await.unwrap_or_default()
    );
    assert_eq!(
        get(&client, format!("http://127.0.0.1:{port}/added")).await,
        (200, "from-stub-door".to_owned())
    );

    // And resolution wrote the chosen engine back, so the persisted form is unambiguous.
    let imposter: serde_json::Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET imposter")
        .json()
        .await
        .expect("json");
    for stub in imposter["stubs"].as_array().expect("stubs") {
        assert_eq!(
            stub["responses"][0]["_rift"]["script"]["engine"],
            "javascript"
        );
    }
    let _ = manager.delete_imposter(port).await;
}

/// A leftover `"lua"` default with no engine-less script still loads — refusing it would break
/// configs that load today. Only a script that actually needs it hits the existing #450 error.
#[tokio::test]
async fn an_unknown_default_fails_only_the_script_that_needs_it() {
    let (client, admin, manager) = start_admin().await;
    let port = free_port();
    let unused = client
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "port": port,
            "protocol": "http",
            "_rift": { "scriptEngine": { "defaultEngine": "lua" } },
            "stubs": [{ "responses": [{ "_rift": { "script": {
                "engine": "rhai", "code": "fn respond(ctx) { http(200, \"ok\") }"
            } } }] }]
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(
        unused.status().as_u16(),
        201,
        "{}",
        unused.text().await.unwrap_or_default()
    );
    let _ = manager.delete_imposter(port).await;

    let port = free_port();
    let used = client
        .post(format!("{admin}/imposters"))
        .json(&serde_json::json!({
            "port": port,
            "protocol": "http",
            "_rift": { "scriptEngine": { "defaultEngine": "lua" } },
            "stubs": [{ "responses": [js_script("never")] }]
        }))
        .send()
        .await
        .expect("POST");
    let status = used.status().as_u16();
    let body = used.text().await.unwrap_or_default();
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("450"),
        "the existing #450 error names the removal: {body}"
    );
    let _ = manager.delete_imposter(port).await;
}
