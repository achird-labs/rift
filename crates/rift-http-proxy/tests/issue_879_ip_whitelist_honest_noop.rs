//! Issue #879: `--ip-whitelist` was declared and read nowhere, while four separate places — the CLI
//! reference table, a "Restrict access" example promising CIDR, the Node getting-started page, and
//! the Mountebank compatibility matrix marking it ✅ Complete — told operators it worked.
//!
//! The resolution is to make it an **honest no-op** rather than to implement it: a prior decision
//! already exists (`tests/compatibility/COMPATIBILITY_COVERAGE.md` records "use Kubernetes
//! NetworkPolicy instead"), the repo already has a house pattern for accepted-but-unimplemented
//! Mountebank flags (`--formatter`, `--protofile`), and a network ACL enforced inside the mock
//! server is strictly weaker than the same ACL in the network — behind any proxy or container NAT
//! the peer address is the hop, not the client.
//!
//! A silently-inert security flag is the worst of the three options. This suite pins that it is now
//! either honest or absent, never quietly pretending.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};

// The Mountebank compatibility guarantee: the flag still parses and the server still starts. This is
// why it is kept as a no-op rather than deleted — removing it would break anyone passing it.
#[tokio::test]
async fn the_flag_is_still_accepted_and_the_server_still_starts() {
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--ip-whitelist",
        "10.0.0.1,192.168.0.1",
    ]);
    assert_eq!(
        cli.ip_whitelist.as_deref(),
        Some(&["10.0.0.1".to_string(), "192.168.0.1".to_string()][..]),
        "the flag must keep parsing its comma-separated value"
    );

    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("an accepted no-op must not fail startup");
    server.shutdown().await;
}

// `GET /config` reported `"localOnly": false` as a hardcoded literal. That was always inaccurate and
// became actively wrong once #863/#880 gave `--local-only` real meaning on two listeners — an
// endpoint answering a question about a network control with a fixed string is the same class of
// problem as the flag this issue is about.
#[tokio::test]
async fn get_config_reports_the_real_bind_posture() {
    let loopback = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("start");

    let config: serde_json::Value =
        reqwest::get(format!("http://{}/config", loopback.admin_addr()))
            .await
            .expect("GET /config")
            .json()
            .await
            .expect("json");
    assert_eq!(
        config["options"]["localOnly"], true,
        "a loopback-bound admin plane must report localOnly: true"
    );
    // Accurate *because* nothing is filtered — the flag is a documented no-op, so "all addresses
    // may connect" is now the truth rather than a placeholder.
    assert_eq!(config["options"]["ipWhitelist"], serde_json::json!(["*"]));
    loopback.shutdown().await;

    let exposed = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--host",
        "0.0.0.0",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("start");

    let config: serde_json::Value = reqwest::get(format!("http://{}/config", exposed.admin_addr()))
        .await
        .expect("GET /config")
        .json()
        .await
        .expect("json");
    assert_eq!(
        config["options"]["localOnly"], false,
        "an off-host admin plane must report localOnly: false"
    );
    exposed.shutdown().await;
}

// The discriminating case, and the reason `localOnly` reports the FLAG rather than "did the admin
// plane bind loopback". `--host 127.0.0.1` without `--local-only` binds the admin plane to loopback
// — but `/metrics` and every imposter port stay on `0.0.0.0`. A bind-derived `true` here would tell
// an operator nothing is reachable off-host while two listener families are, which overstates the
// restriction. The old hardcoded `false` understated; overstating is the direction that gets someone
// hurt, so the flag is what gets reported.
#[tokio::test]
async fn a_loopback_host_without_the_flag_does_not_claim_local_only() {
    let server = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("start");

    let config: serde_json::Value = reqwest::get(format!("http://{}/config", server.admin_addr()))
        .await
        .expect("GET /config")
        .json()
        .await
        .expect("json");
    assert_eq!(
        config["options"]["localOnly"], false,
        "--host 127.0.0.1 narrows only the admin plane; metrics and imposter ports stay off-host, \
         so reporting localOnly: true would overstate the restriction"
    );

    server.shutdown().await;
}

// `port` was hardcoded to 2525 in the same object, so `--port 0` (what every embedder test uses)
// reported a port nothing was listening on. Fixed alongside `localOnly` because the CHANGELOG claims
// the object reports the real posture — a claim one hardcoded sibling would falsify.
#[tokio::test]
async fn get_config_reports_the_real_admin_port() {
    let server = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("start");
    let bound = server.admin_addr().port();

    let config: serde_json::Value = reqwest::get(format!("http://{}/config", server.admin_addr()))
        .await
        .expect("GET /config")
        .json()
        .await
        .expect("json");
    assert_eq!(
        config["options"]["port"], bound,
        "the reported port must be the one actually bound, not the 2525 default"
    );

    server.shutdown().await;
}

// Passing the flag must not change the reported posture either — it does nothing, and `GET /config`
// must not start implying otherwise.
#[tokio::test]
async fn passing_the_flag_does_not_change_the_reported_whitelist() {
    let server = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--ip-whitelist",
        "10.0.0.1",
    ]))
    .start()
    .await
    .expect("start");

    let config: serde_json::Value = reqwest::get(format!("http://{}/config", server.admin_addr()))
        .await
        .expect("GET /config")
        .json()
        .await
        .expect("json");
    assert_eq!(
        config["options"]["ipWhitelist"],
        serde_json::json!(["*"]),
        "the flag filters nothing, so the reported whitelist must stay open — reporting the \
         supplied list would re-create the false assurance this issue removes"
    );

    server.shutdown().await;
}
