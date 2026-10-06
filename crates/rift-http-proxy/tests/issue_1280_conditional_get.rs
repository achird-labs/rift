//! Issue #1280: `_rift.conditional` through the server — the admin API echoes it as declared, and a
//! reload moves the validators only for a stub that changed (an unchanged stub keeps its state
//! since #1265, so its `Last-Modified` stays put).

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::json;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Two conditional stubs: `/changing` answers `changing_body`, `/stable` always answers `stable`.
fn write_config(path: &Path, imposter_port: u16, changing_body: &str) {
    let config = json!({ "imposters": [{ "port": imposter_port, "protocol": "http", "stubs": [
        { "predicates": [{ "equals": { "path": "/changing" } }],
          "responses": [{ "is": { "body": changing_body }, "_rift": { "conditional": true } }] },
        { "predicates": [{ "equals": { "path": "/stable" } }],
          "responses": [{ "is": { "body": "stable" }, "_rift": { "conditional": true } }] }
    ] }] });
    std::fs::write(path, config.to_string()).expect("write config");
}

async fn start(args: &[&str]) -> RunningServer {
    let mut argv = vec![
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ];
    argv.extend_from_slice(args);
    ServerBuilder::from_cli(Cli::try_parse_from(argv).expect("cli"))
        .start()
        .await
        .expect("start")
}

/// Start on a config file whose imposter got its port, retrying on a fresh port when a parallel
/// test took the one `free_port` released.
async fn start_with_config(config: &Path, changing_body: &str) -> (RunningServer, u16) {
    for _ in 0..5 {
        let imposter = free_port();
        write_config(config, imposter, changing_body);
        let server = start(&["--configfile", config.to_str().expect("utf8")]).await;
        let bound = reqwest::get(format!("http://127.0.0.1:{imposter}/stable"))
            .await
            .is_ok_and(|r| r.status().is_success());
        if bound {
            return (server, imposter);
        }
        server.shutdown().await;
    }
    panic!("no free port for the imposter after 5 attempts");
}

async fn reload(server: &RunningServer) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", server.admin_addr()))
        .send()
        .await
        .expect("reload");
    assert_eq!(response.status(), 200);
}

/// `(ETag, Last-Modified)` of a plain GET.
async fn validators(port: u16, path: &str) -> (String, String) {
    let response = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("request");
    assert_eq!(response.status(), 200);
    let get = |name: &str| {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("{name} on {path}"))
            .to_str()
            .expect("ascii")
            .to_owned()
    };
    (get("etag"), get("last-modified"))
}

fn assert_etag_format(etag: &str) {
    let re = regex::Regex::new(r#"^"fnv1a64-[0-9a-f]{16}"$"#).expect("valid regex");
    assert!(re.is_match(etag), "not a rift ETag: {etag}");
}

#[tokio::test]
async fn reload_with_changed_body_moves_etag_and_last_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let (server, port) = start_with_config(&config, "v1").await;
    let (etag_before, modified_before) = validators(port, "/changing").await;
    assert_eq!(etag_before, "\"fnv1a64-08cf0b07b5709128\"");

    // Last-Modified has second granularity.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    write_config(&config, port, "v2");
    reload(&server).await;

    let (etag_after, modified_after) = validators(port, "/changing").await;
    assert_etag_format(&etag_after);
    assert_eq!(etag_after, "\"fnv1a64-08cf0e07b5709641\"");
    assert_ne!(etag_after, etag_before);
    assert_ne!(modified_after, modified_before);

    let revalidated = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/changing"))
        .header("If-Modified-Since", modified_before.as_str())
        .send()
        .await
        .expect("request");
    assert_eq!(revalidated.status(), 200, "the old stamp is stale now");
    server.shutdown().await;
}

#[tokio::test]
async fn reload_with_unchanged_stub_keeps_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("imposters.json");
    let (server, port) = start_with_config(&config, "v1").await;
    let before = validators(port, "/stable").await;
    assert_eq!(before.0, "\"fnv1a64-3f63b56db2890a16\"");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    // The sibling stub changes, so the reload has a diff to apply; `/stable` does not.
    write_config(&config, port, "v2");
    reload(&server).await;

    assert_eq!(validators(port, "/stable").await, before);
    server.shutdown().await;
}

#[tokio::test]
async fn get_imposters_echoes_conditional_exactly_as_declared() {
    let server = start(&[]).await;
    let admin = server.admin_addr();
    let shapes = [
        json!(true),
        json!({ "etag": true, "lastModified": "load" }),
        json!({ "etag": false, "lastModified": "Sat, 03 Oct 2026 12:00:00 GMT" }),
        json!({ "lastModified": "load" }),
    ];
    let stubs: Vec<serde_json::Value> = shapes
        .iter()
        .map(|shape| json!({ "responses": [{ "is": { "body": "x" }, "_rift": { "conditional": shape } }] }))
        .collect();
    let port = free_port();
    let created = reqwest::Client::new()
        .post(format!("http://{admin}/imposters"))
        .json(&json!({ "port": port, "protocol": "http", "stubs": stubs }))
        .send()
        .await
        .expect("create");
    assert_eq!(
        created.status(),
        201,
        "{}",
        created.text().await.unwrap_or_default()
    );

    let echoed: serde_json::Value = reqwest::get(format!("http://{admin}/imposters/{port}"))
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    for (i, shape) in shapes.iter().enumerate() {
        assert_eq!(
            echoed["stubs"][i]["responses"][0]["_rift"]["conditional"], *shape,
            "{echoed}"
        );
    }

    let refused = reqwest::Client::new()
        .post(format!("http://{admin}/imposters"))
        .json(
            &json!({ "port": free_port(), "protocol": "http", "stubs": [{ "responses": [
            { "is": { "body": "x" }, "_rift": { "conditional": { "lastModified": "yesterday" } } }
        ] }] }),
        )
        .send()
        .await
        .expect("create");
    assert_eq!(
        refused.status(),
        400,
        "a bad fixed date is refused at admission"
    );
    server.shutdown().await;
}
