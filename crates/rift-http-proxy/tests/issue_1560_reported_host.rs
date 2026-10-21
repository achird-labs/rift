//! Issue #1560: `GET /config` reports `options.host` from the CLI's `--host`. An embedder that
//! re-points `--host` for its own listener (rift-cluster pins it to loopback for its internal
//! admin leg) therefore advertised the private value. `ServerBuilder::reported_host` is the host's
//! version of `reported_admin_port` (#1135): report-only, the bind is untouched.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};

async fn reported_options_host(builder: ServerBuilder) -> serde_json::Value {
    let server = builder.start().await.expect("server starts");
    let addr = server.admin_addr();
    // The admin plane must still answer on loopback regardless of what is reported.
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/config"))
        .await
        .expect("admin API reachable on its real bind")
        .json()
        .await
        .expect("json");
    server.shutdown().await;
    body["options"]["host"].clone()
}

fn cli() -> Cli {
    Cli::parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
}

// Pins the default: with no override, `/config` reports the CLI's `--host`.
#[tokio::test]
async fn without_the_override_config_reports_the_cli_host() {
    let host = reported_options_host(ServerBuilder::from_cli(cli())).await;
    assert_eq!(host, "127.0.0.1");
}

#[tokio::test]
async fn the_override_is_what_config_reports() {
    let host = reported_options_host(
        ServerBuilder::from_cli(cli()).reported_host(Some("10.0.0.5".into())),
    )
    .await;
    assert_eq!(host, "10.0.0.5");
}

// The inner `Option` is a value: an embedder can report "no host" even though the CLI had one.
#[tokio::test]
async fn an_override_of_none_reports_no_host() {
    let host = reported_options_host(ServerBuilder::from_cli(cli()).reported_host(None)).await;
    assert!(host.is_null(), "expected no host, got {host}");
}
