//! Issue #1615: `GET /logs` returned `[]` forever; Mountebank returns its log entries as
//! `{logs:[{level,message,timestamp}]}`. Rift serves them from a bounded in-memory ring installed
//! as a `tracing` layer.

use clap::Parser;
use rift_http_proxy::admin_api::LogRing;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::sync::OnceLock;
use tracing_subscriber::prelude::*;

/// One process-wide ring: the global subscriber can be installed once per test binary.
fn ring() -> &'static LogRing {
    static RING: OnceLock<LogRing> = OnceLock::new();
    RING.get_or_init(|| {
        let ring = LogRing::new();
        tracing_subscriber::registry()
            .with(ring.clone())
            .try_init()
            .expect("first and only global subscriber in this test binary");
        ring
    })
}

fn cli(extra: &[&str]) -> Cli {
    let mut args = vec![
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ];
    args.extend_from_slice(extra);
    Cli::parse_from(args)
}

async fn get_logs(addr: std::net::SocketAddr, query: &str, key: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("http://{addr}/logs{query}"));
    if let Some(key) = key {
        req = req.header("x-api-key", key);
    }
    req.send().await.expect("admin API answers")
}

#[tokio::test]
async fn logs_return_recent_entries_in_mountebanks_shape() {
    let server = ServerBuilder::from_cli(cli(&[]))
        .log_ring(ring().clone())
        .start()
        .await
        .expect("server starts");
    let addr = server.admin_addr();

    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{addr}/imposters"))
        .json(&serde_json::json!({"protocol": "http", "stubs": []}))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("json");
    let port = created["port"].as_u64().expect("assigned port");

    let body: serde_json::Value = get_logs(addr, "", None).await.json().await.expect("json");
    let top: Vec<_> = body.as_object().expect("object").keys().collect();
    assert_eq!(top, ["logs"], "no `_links`: Mountebank sends none");
    let logs = body["logs"].as_array().expect("array");
    for entry in logs {
        let mut keys: Vec<_> = entry.as_object().expect("entry").keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["level", "message", "timestamp"], "{entry}");
    }
    let created_line = logs
        .iter()
        .position(|e| {
            e["level"] == "info"
                && e["message"]
                    .as_str()
                    .is_some_and(|m| m.contains(&format!("Created imposter on port {port}")))
        })
        .unwrap_or_else(|| panic!("no 'Created imposter on port {port}' entry in {body}"));

    // `endIndex` is inclusive and indices are absolute: one entry for startIndex == endIndex.
    let one: serde_json::Value = get_logs(
        addr,
        &format!("?startIndex={created_line}&endIndex={created_line}"),
        None,
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(one["logs"].as_array().expect("array").len(), 1);
    assert_eq!(one["logs"][0], logs[created_line]);

    // Mountebank: `endIndex=abc` slices to nothing.
    let none: serde_json::Value = get_logs(addr, "?endIndex=abc", None)
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(none["logs"], serde_json::json!([]));

    server.shutdown().await;
}

#[tokio::test]
async fn a_server_without_a_ring_answers_the_no_logfile_sentinel() {
    let server = ServerBuilder::from_cli(cli(&[]))
        .start()
        .await
        .expect("server starts");
    let body: serde_json::Value = get_logs(server.admin_addr(), "", None)
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(
        body,
        serde_json::json!({"logs": [{"level": "error", "message": "No logfile"}]})
    );
    server.shutdown().await;
}

#[tokio::test]
async fn logs_sit_behind_the_admin_api_key_and_leak_neither_script_output_nor_line_breaks() {
    let server = ServerBuilder::from_cli(cli(&["--api-key", "k3y-for-1615"]))
        .log_ring(ring().clone())
        .start()
        .await
        .expect("server starts");
    let addr = server.admin_addr();

    // A script's own output (target `rift::script`) can echo request secrets: never recorded.
    tracing::info!(target: "rift::script", "Authorization: Bearer s3cret-from-a-script");
    // A CR/LF in a message must reach the client as an escaped JSON string.
    tracing::warn!("first\r\nsecond forged: {{\"level\":\"error\"}}");

    assert_eq!(get_logs(addr, "", None).await.status(), 401, "no key");
    assert_eq!(
        get_logs(addr, "", Some("wrong")).await.status(),
        401,
        "wrong key"
    );

    let resp = get_logs(addr, "", Some("k3y-for-1615")).await;
    assert_eq!(resp.status(), 200);
    let raw = resp.text().await.expect("body");
    assert!(
        !raw.contains("s3cret-from-a-script"),
        "script output leaked"
    );
    // The body is pretty-printed, so it has line breaks of its own; the message's CR/LF must not
    // be among them.
    assert!(!raw.contains('\r'), "raw CR in {raw}");
    assert!(
        !raw.lines().any(|l| l.trim_start().starts_with("second")),
        "the message was split across body lines: {raw}"
    );
    assert!(
        raw.contains(r"first\r\nsecond"),
        "escaped CR/LF expected in {raw}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(parsed["logs"].as_array().expect("array").iter().any(|e| {
        e["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("first\r\nsecond"))
    }));
    server.shutdown().await;
}
