//! Issue #1528: with no `--host`, Mountebank's admin listener is Node's `listen(port)`, which binds
//! `[::]` dual-stack. Rift bound `0.0.0.0`, so on an IPv6 single-stack Kubernetes cluster the
//! `httpGet` probes on the pod's IPv6 address failed and `/metrics` was unscrapeable.
//!
//! These tests pin the server-level listeners: the admin API, `/metrics` and a bare-port
//! `--front-door` answer on both families when no `--host` is given; `--local-only` answers on
//! `127.0.0.1` and `[::1]`; an explicit `--host` binds exactly that address. Every port is `0`.
//! The IPv6 halves are skipped on a host with no IPv6 loopback.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use serial_test::serial;
use std::net::SocketAddr;

/// Whether this host can listen on `[::1]` at all; CI runners without IPv6 skip the v6 halves.
fn has_ipv6_loopback() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("client")
}

async fn status(url: String) -> Result<u16, String> {
    client()
        .get(&url)
        .send()
        .await
        .map(|r| r.status().as_u16())
        .map_err(|e| format!("GET {url}: {e}"))
}

/// Both loopback families answer `path` on `port` with a 2xx.
async fn assert_both_families_answer(port: u16, path: &str) {
    let v4 = status(format!("http://127.0.0.1:{port}{path}")).await;
    assert_eq!(v4, Ok(200), "127.0.0.1:{port}{path} must answer");
    if has_ipv6_loopback() {
        let v6 = status(format!("http://[::1]:{port}{path}")).await;
        assert_eq!(v6, Ok(200), "[::1]:{port}{path} must answer");
    }
}

#[tokio::test]
#[serial]
async fn with_no_host_admin_and_metrics_answer_on_ipv4_and_ipv6() {
    let cli = Cli::parse_from(["rift", "--port", "0", "--metrics-port", "0"]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");

    let admin = server.admin_addr();
    let metrics = server.metrics_addr().expect("metrics bound");
    assert_both_families_answer(admin.port(), "/health").await;
    assert_both_families_answer(metrics.port(), "/metrics").await;

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn local_only_answers_on_both_loopbacks() {
    let cli = Cli::parse_from(["rift", "--local-only", "--port", "0", "--metrics-port", "0"]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");

    let admin = server.admin_addr();
    let metrics = server.metrics_addr().expect("metrics bound");
    assert!(admin.ip().is_loopback(), "admin {admin}");
    assert!(metrics.ip().is_loopback(), "metrics {metrics}");
    assert_both_families_answer(admin.port(), "/health").await;
    assert_both_families_answer(metrics.port(), "/metrics").await;

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn an_explicit_ipv4_host_binds_exactly_that_address() {
    let cli = Cli::parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    let port = server.admin_addr().port();
    assert_eq!(
        server.admin_addr(),
        SocketAddr::from(([127, 0, 0, 1], port))
    );
    assert_eq!(
        status(format!("http://127.0.0.1:{port}/health")).await,
        Ok(200)
    );
    if has_ipv6_loopback() {
        assert!(
            status(format!("http://[::1]:{port}/health")).await.is_err(),
            "an explicit --host 127.0.0.1 must not also listen on [::1]"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn a_bare_port_front_door_answers_on_ipv4_and_ipv6() {
    let cli = Cli::parse_from([
        "rift",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--front-door",
        "0",
    ]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    let port = server.front_door_addr().expect("front door bound").port();

    // No route is configured, so the front door answers 404 — what matters is that it answers.
    let v4 = status(format!("http://127.0.0.1:{port}/")).await;
    assert_eq!(v4, Ok(404), "127.0.0.1 front door");
    if has_ipv6_loopback() {
        let v6 = status(format!("http://[::1]:{port}/")).await;
        assert_eq!(v6, Ok(404), "[::1] front door");
    }
    server.shutdown().await;
}

/// `GET /config` reports the `--host` flag, not the bind: `null` when unset.
#[tokio::test]
#[serial]
async fn config_reports_the_host_flag_not_the_bind() {
    for (argv, expected) in [
        (vec!["rift"], serde_json::Value::Null),
        (
            vec!["rift", "--host", "127.0.0.1"],
            serde_json::json!("127.0.0.1"),
        ),
    ] {
        let mut argv = argv;
        argv.extend(["--port", "0", "--metrics-port", "0"]);
        let server = ServerBuilder::from_cli(Cli::parse_from(&argv))
            .start()
            .await
            .expect("start");
        let port = server.admin_addr().port();
        let config: serde_json::Value = client()
            .get(format!("http://127.0.0.1:{port}/config"))
            .send()
            .await
            .expect("GET /config")
            .json()
            .await
            .expect("json");
        assert_eq!(config["options"]["host"], expected, "{argv:?}");
        server.shutdown().await;
    }
}

/// `rift healthcheck` with no `--host` probes `127.0.0.1`, which answers on a dual-stack bind.
#[tokio::test]
#[serial]
async fn healthcheck_with_no_host_reaches_a_dual_stack_server() {
    let cli = Cli::parse_from(["rift", "--port", "0", "--metrics-port", "0"]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    let port = server.admin_addr().port().to_string();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_rift-http-proxy"))
        .args(["healthcheck", "--port", &port])
        .env_remove("MB_HOST")
        .output()
        .await
        .expect("run rift healthcheck");
    assert!(
        output.status.success(),
        "healthcheck failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.shutdown().await;
}

/// `MB_HOST` is the env spelling of `--host`, so it counts as explicit: an rcfile `host` does not
/// override it. Every test in this binary is `#[serial]`, so no other test parses a `Cli` while the
/// variable is set.
#[test]
#[serial]
fn mb_host_counts_as_an_explicit_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = dir.path().join("rift.rc");
    std::fs::write(&rcfile, r#"{"host": "10.0.0.1"}"#).expect("write rcfile");

    // SAFETY: serialised with every other test in this binary; nothing else reads the environment.
    unsafe { std::env::set_var("MB_HOST", "0.0.0.0") };
    let parsed = Cli::try_parse_from(["rift"]);
    unsafe { std::env::remove_var("MB_HOST") };
    let mut cli = parsed.expect("cli parse");

    rift_http_proxy::bootstrap::apply_rcfile_defaults(&mut cli, &rcfile).expect("applies");
    assert_eq!(cli.host.as_deref(), Some("0.0.0.0"));
}
