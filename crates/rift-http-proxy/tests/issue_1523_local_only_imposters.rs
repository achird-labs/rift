//! Issue #1523: Mountebank's `--localOnly` refuses every client outside the loopback interface on
//! the admin API **and every imposter** (`util/ip.js:5-52`, `models/imposter.js:103-107`). Rift's
//! `--local-only` only pinned the admin API and `/metrics` to loopback, so imposters, and every
//! imposter behind the front door, stayed reachable from other hosts.
//!
//! These tests pin the refusal on an imposter's own port and on the front door, the
//! `rift_blocked_connections_total{listener}` counter, that nothing changes without the flag, and
//! that a manager injected through `ServerBuilder::manager` keeps its own policy (rift-cluster
//! injects one). Every port is OS-assigned.
//!
//! Issue #1568 extends this to the admin API and `/metrics`: under `--local-only` they bind loopback,
//! but a loopback bind still accepts a connection whose *source* is another local interface's
//! address (the kernel routes it over `lo0`). Mountebank refuses it on the admin server's
//! `connection` event (`mountebank.js:228-229`); Rift now refuses it at accept too.
//!
//! "Remote" is this host's own off-host address (#1517's UDP trick), which is not loopback. A host
//! with no such address skips the test with a message.

use clap::Parser;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use serial_test::serial;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const V4_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The address this host routes off-host traffic from. A UDP `connect` sends nothing; it only picks
/// the source address. `None` on a host with no route out, where only loopback exists.
fn non_loopback_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn remote_or_skip(test: &str) -> Option<IpAddr> {
    let ip = non_loopback_local_ip();
    if ip.is_none() {
        eprintln!("{test}: skipped, this host has no non-loopback address");
    }
    ip
}

/// One `GET path` over a raw socket, returning whatever came back. A refused client's connection
/// is closed with nothing written, which reads as EOF or a reset depending on timing: either way
/// the result is empty.
async fn raw_get(ip: IpAddr, port: u16, path: &str) -> String {
    let connect = tokio::net::TcpStream::connect(SocketAddr::new(ip, port));
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_secs(2), connect).await else {
        return String::new();
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    match tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await {
        Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => panic!("{ip}:{port}{path}: the connection neither answered nor closed"),
    }
}

/// [`raw_get`] to `127.0.0.1:port` from a client socket bound to `source` (issue #1568). The
/// destination is loopback, so the connection rides `lo0` and reaches a loopback-bound listener;
/// only its source address is non-loopback.
async fn raw_get_from(source: IpAddr, port: u16, path: &str) -> String {
    let socket = match source {
        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
        IpAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
    }
    .expect("client socket");
    socket
        .bind(SocketAddr::new(source, 0))
        .expect("bind the client to the non-loopback source");
    let destination = match source {
        IpAddr::V4(_) => V4_LOOPBACK,
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    };
    let connect = socket.connect(SocketAddr::new(destination, port));
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_secs(2), connect).await else {
        return String::new();
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    match tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await {
        Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => panic!("{source} -> {destination}:{port}{path}: neither answered nor closed"),
    }
}

fn served(response: &str) -> bool {
    response.starts_with("HTTP/1.1 200")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
}

async fn start(argv: &[&str]) -> RunningServer {
    let mut full = vec!["rift"];
    full.extend_from_slice(argv);
    full.extend(["--port", "0", "--metrics-port", "0"]);
    ServerBuilder::from_cli(Cli::try_parse_from(&full).expect("cli parse"))
        .start()
        .await
        .expect("start")
}

fn imposter() -> Value {
    json!({
        "protocol": "http",
        "host": "0.0.0.0",
        "stubs": [{"responses": [{"is": {"body": "ok"}}]}]
    })
}

/// `POST /imposters` and the port it bound.
async fn create(server: &RunningServer) -> u16 {
    let response = client()
        .post(format!(
            "http://127.0.0.1:{}/imposters",
            server.admin_addr().port()
        ))
        .json(&imposter())
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status().as_u16(), 201, "create");
    let created: Value = response.json().await.expect("json");
    u16::try_from(created["port"].as_u64().expect("port")).expect("port fits u16")
}

/// Delete every imposter, then stop the server: the imposters live in its manager and would keep
/// their ports for the rest of this binary.
async fn stop(server: &RunningServer) {
    let response = client()
        .delete(format!(
            "http://127.0.0.1:{}/imposters",
            server.admin_addr().port()
        ))
        .send()
        .await
        .expect("DELETE /imposters");
    assert!(response.status().is_success(), "DELETE /imposters");
    server.shutdown().await;
}

/// `rift_blocked_connections_total{listener}` from this server's `/metrics`, `None` while the
/// series does not exist. The registry is process-wide, so callers compare before and after.
async fn blocked(server: &RunningServer, listener: &str) -> Option<u64> {
    let metrics = server.metrics_addr().expect("metrics bound");
    let scrape = client()
        .get(format!("http://127.0.0.1:{}/metrics", metrics.port()))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("metrics text");
    let series = format!("rift_blocked_connections_total{{listener=\"{listener}\"}} ");
    scrape
        .lines()
        .find_map(|line| line.strip_prefix(series.as_str()))
        .map(|n| n.trim().parse().expect("counter value"))
}

#[tokio::test]
#[serial]
async fn local_only_refuses_remote_clients_on_imposters_and_the_front_door() {
    let Some(remote) = remote_or_skip("local only") else {
        return;
    };
    let server = start(&["--local-only", "--front-door", "0.0.0.0:0"]).await;
    let port = create(&server).await;
    let front_door = server.front_door_addr().expect("front door bound").port();
    let gateway = format!("/__rift/{port}/x");

    let imposter_before = blocked(&server, "imposter").await;
    let front_door_before = blocked(&server, "front-door").await;

    let local = raw_get(V4_LOOPBACK, port, "/x").await;
    assert!(served(&local), "127.0.0.1:{port} must be served: {local:?}");
    let local = raw_get(V4_LOOPBACK, front_door, &gateway).await;
    assert!(
        served(&local),
        "the front door must serve loopback: {local:?}"
    );

    let refused = raw_get(remote, port, "/x").await;
    assert!(
        refused.is_empty(),
        "{remote}:{port} must get a closed connection under --local-only, got {refused:?}"
    );
    let refused = raw_get(remote, front_door, &gateway).await;
    assert!(
        refused.is_empty(),
        "the front door {remote}:{front_door} must refuse under --local-only, got {refused:?}"
    );

    // Materialised at zero under `--local-only`, so "nothing refused" reads differently from "not
    // enforced".
    let imposter_before = imposter_before.expect("the imposter series exists under --local-only");
    let front_door_before =
        front_door_before.expect("the front-door series exists under --local-only");
    assert_eq!(
        blocked(&server, "imposter").await,
        Some(imposter_before + 1),
        "one imposter refusal is counted"
    );
    assert_eq!(
        blocked(&server, "front-door").await,
        Some(front_door_before + 1),
        "one front-door refusal is counted"
    );
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn without_local_only_remote_clients_are_served() {
    let Some(remote) = remote_or_skip("no local only") else {
        return;
    };
    let server = start(&["--front-door", "0.0.0.0:0"]).await;
    let port = create(&server).await;
    let front_door = server.front_door_addr().expect("front door bound").port();

    let response = raw_get(remote, port, "/x").await;
    assert!(
        served(&response),
        "{remote}:{port} must be served: {response:?}"
    );
    let response = raw_get(remote, front_door, &format!("/__rift/{port}/x")).await;
    assert!(
        served(&response),
        "the front door must serve {remote}: {response:?}"
    );
    stop(&server).await;
}

/// An embedder injecting its own manager keeps its own peer policy: rift-cluster runs nodes behind
/// a load balancer, and `--local-only` there confines its admin, not the cluster's imposters.
#[tokio::test]
#[serial]
async fn an_injected_manager_does_not_inherit_local_only() {
    let Some(remote) = remote_or_skip("injected") else {
        return;
    };
    let manager = Arc::new(ImposterManager::new());
    let cli = Cli::try_parse_from(["rift", "--local-only", "--port", "0", "--metrics-port", "0"])
        .expect("cli parse");
    let server = ServerBuilder::from_cli(cli)
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("start");

    let config: ImposterConfig = serde_json::from_value(imposter()).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let response = raw_get(remote, port, "/x").await;
    assert!(
        served(&response),
        "{remote}:{port} must be served by an injected manager: {response:?}"
    );

    server.shutdown().await;
    manager.delete_all().await;
}

/// Issue #1568: the admin API and `/metrics` are bound to loopback under `--local-only`, but that
/// alone admits a non-loopback source arriving over loopback. Both now refuse it, and count it.
#[tokio::test]
#[serial]
async fn local_only_refuses_a_non_loopback_source_on_admin_and_metrics() {
    let Some(remote) = remote_or_skip("admin and metrics") else {
        return;
    };
    let server = start(&["--local-only"]).await;
    let admin = server.admin_addr().port();
    let metrics = server.metrics_addr().expect("metrics bound").port();

    let admin_before = blocked(&server, "admin").await;
    let metrics_before = blocked(&server, "metrics").await;

    let local = raw_get(V4_LOOPBACK, admin, "/imposters").await;
    assert!(
        served(&local),
        "the admin API must serve loopback: {local:?}"
    );
    let local = raw_get(V4_LOOPBACK, metrics, "/metrics").await;
    assert!(served(&local), "/metrics must serve loopback: {local:?}");

    let refused = raw_get_from(remote, admin, "/imposters").await;
    assert!(
        refused.is_empty(),
        "a client from {remote} must get a closed admin connection under --local-only, got \
         {refused:?}"
    );
    let refused = raw_get_from(remote, metrics, "/metrics").await;
    assert!(
        refused.is_empty(),
        "a client from {remote} must get a closed /metrics connection under --local-only, got \
         {refused:?}"
    );

    // Materialised at zero under `--local-only`, as the imposter and front-door series are.
    let admin_before = admin_before.expect("the admin series exists under --local-only");
    let metrics_before = metrics_before.expect("the metrics series exists under --local-only");
    assert_eq!(
        blocked(&server, "admin").await,
        Some(admin_before + 1),
        "one admin refusal is counted"
    );
    assert_eq!(
        blocked(&server, "metrics").await,
        Some(metrics_before + 1),
        "one metrics refusal is counted"
    );
    server.shutdown().await;
}

/// Without `--local-only` a loopback-bound admin plane (`--host 127.0.0.1`) still serves a
/// non-loopback source: the bind narrows, the peer policy stays `Any` (issue #1568).
#[tokio::test]
#[serial]
async fn without_local_only_a_loopback_admin_serves_a_non_loopback_source() {
    let Some(remote) = remote_or_skip("admin without local only") else {
        return;
    };
    let server = start(&["--host", "127.0.0.1"]).await;
    let admin = server.admin_addr().port();
    let metrics = server.metrics_addr().expect("metrics bound").port();

    let response = raw_get_from(remote, admin, "/imposters").await;
    assert!(
        served(&response),
        "the admin API must serve {remote} without --local-only: {response:?}"
    );
    let response = raw_get_from(remote, metrics, "/metrics").await;
    assert!(
        served(&response),
        "/metrics must serve {remote} without --local-only: {response:?}"
    );
    server.shutdown().await;
}
