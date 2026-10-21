//! Issue #1572: under `--local-only` the intercept listener bound `127.0.0.1` only and had no peer
//! check. The admin API and `/metrics` listen on `127.0.0.1` and `[::1]` (#1528) and refuse a
//! non-loopback client at accept (#1568); the intercept listener now does both.
//!
//! - `--local-only --intercept-port 0` answers a `CONNECT` on `[::1]` as well as on `127.0.0.1`
//!   (skipped on a host with no IPv6 loopback, as the #1528 tests are).
//! - A non-loopback source routed over loopback is closed at accept and counted in
//!   `rift_blocked_connections_total{listener="intercept"}`.
//! - A runtime `POST /intercept {"host": "0.0.0.0"}` under `--local-only` still refuses off-host
//!   clients, the way an imposter naming its own `host` does (#1523).
//! - Without `--local-only` nothing is refused.
//!
//! "Remote" is this host's own off-host address (#1517's UDP trick), which is not loopback. A host
//! with no such address skips those tests with a message. Every port is OS-assigned.

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use serial_test::serial;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
const ESTABLISHED: &str = "HTTP/1.1 200 Connection Established";

/// Whether this host can listen on `[::1]` at all; CI runners without IPv6 skip the v6 halves.
fn has_ipv6_loopback() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
}

async fn start(argv: &[&str]) -> RunningServer {
    let mut full = vec!["rift", "--port", "0", "--metrics-port", "0"];
    full.extend_from_slice(argv);
    ServerBuilder::from_cli(Cli::try_parse_from(&full).expect("cli parse"))
        .start()
        .await
        .expect("start")
}

/// Send one `CONNECT` on `stream` and return the first bytes of the reply. A refused client's
/// connection is closed with nothing written, which reads as EOF or a reset: either way, empty.
/// The listener answers `200 Connection Established` and then waits for TLS, so this reads once
/// rather than to EOF.
async fn connect_reply(mut stream: tokio::net::TcpStream, what: &str) -> String {
    let request = "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\n\r\n";
    if stream.write_all(request.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut buf = vec![0u8; 256];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
        Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).into_owned(),
        Ok(Err(_)) => String::new(),
        Err(_) => panic!("{what}: the intercept listener neither answered nor closed"),
    }
}

/// [`connect_reply`] to `ip:port`; empty when the connection itself is refused.
async fn connect_to(ip: IpAddr, port: u16) -> String {
    let connect = tokio::net::TcpStream::connect(SocketAddr::new(ip, port));
    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(2), connect).await else {
        return String::new();
    };
    connect_reply(stream, &format!("{ip}:{port}")).await
}

/// [`connect_reply`] to `destination:port` from a client socket bound to `source`. With a loopback
/// destination the connection rides `lo0` and reaches a loopback-bound listener; only its source
/// address is non-loopback.
async fn connect_from(source: IpAddr, destination: IpAddr, port: u16) -> String {
    let socket = match source {
        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
        IpAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
    }
    .expect("client socket");
    socket
        .bind(SocketAddr::new(source, 0))
        .expect("bind the client to the non-loopback source");
    let connect = socket.connect(SocketAddr::new(destination, port));
    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(2), connect).await else {
        return String::new();
    };
    connect_reply(stream, &format!("{source} -> {destination}:{port}")).await
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

/// `POST /intercept` with `body`, returning the bound port.
async fn post_intercept(server: &RunningServer, body: Value) -> u16 {
    let response = client()
        .post(format!(
            "http://127.0.0.1:{}/intercept",
            server.admin_addr().port()
        ))
        .json(&body)
        .send()
        .await
        .expect("POST /intercept");
    let status = response.status().as_u16();
    let text = response.text().await.expect("body");
    assert!(
        (200..300).contains(&status),
        "POST /intercept: {status} {text}"
    );
    server.intercept_addr().expect("intercept bound").port()
}

#[tokio::test]
#[serial]
async fn local_only_intercept_answers_on_both_loopbacks() {
    let server = start(&["--local-only", "--intercept-port", "0"]).await;
    let port = server.intercept_addr().expect("intercept bound").port();

    let v4 = connect_to(V4, port).await;
    assert!(v4.starts_with(ESTABLISHED), "127.0.0.1:{port}: {v4:?}");
    if has_ipv6_loopback() {
        let v6 = connect_to(V6, port).await;
        assert!(
            v6.starts_with(ESTABLISHED),
            "[::1]:{port} must answer under --local-only, as admin and /metrics do: {v6:?}"
        );
    } else {
        eprintln!(
            "local_only_intercept_answers_on_both_loopbacks: no IPv6 loopback, v6 half skipped"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn local_only_intercept_refuses_a_non_loopback_source() {
    let Some(remote) = remote_or_skip("intercept local only") else {
        return;
    };
    let server = start(&["--local-only", "--intercept-port", "0"]).await;
    let port = server.intercept_addr().expect("intercept bound").port();
    let before = blocked(&server, "intercept")
        .await
        .expect("the intercept series exists under --local-only");

    let local = connect_to(V4, port).await;
    assert!(
        local.starts_with(ESTABLISHED),
        "loopback is served: {local:?}"
    );
    let destination = if remote.is_ipv4() { V4 } else { V6 };
    let refused = connect_from(remote, destination, port).await;
    assert!(
        refused.is_empty(),
        "a client from {remote} must get a closed intercept connection under --local-only, got \
         {refused:?}"
    );
    assert_eq!(
        blocked(&server, "intercept").await,
        Some(before + 1),
        "one intercept refusal is counted"
    );
    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn local_only_refuses_off_host_clients_of_a_runtime_wildcard_intercept() {
    let Some(remote) = remote_or_skip("runtime intercept local only") else {
        return;
    };
    let server = start(&["--local-only"]).await;
    let port = post_intercept(&server, json!({"host": "0.0.0.0", "port": 0})).await;
    let before = blocked(&server, "intercept")
        .await
        .expect("the intercept series exists under --local-only");

    let local = connect_to(V4, port).await;
    assert!(
        local.starts_with(ESTABLISHED),
        "loopback is served: {local:?}"
    );
    // The wildcard bind accepts on the off-host address itself; only the peer check refuses it.
    let refused = connect_from(remote, remote, port).await;
    assert!(
        refused.is_empty(),
        "a client from {remote} must get a closed connection on a `0.0.0.0` intercept listener \
         under --local-only, got {refused:?}"
    );
    assert_eq!(
        blocked(&server, "intercept").await,
        Some(before + 1),
        "one intercept refusal is counted"
    );
    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn without_local_only_a_loopback_intercept_serves_a_non_loopback_source() {
    let Some(remote) = remote_or_skip("intercept without local only") else {
        return;
    };
    let server = start(&["--host", "127.0.0.1", "--intercept-port", "0"]).await;
    let port = server.intercept_addr().expect("intercept bound").port();
    let destination = if remote.is_ipv4() { V4 } else { V6 };
    let reply = connect_from(remote, destination, port).await;
    assert!(
        reply.starts_with(ESTABLISHED),
        "the intercept listener must serve {remote} without --local-only: {reply:?}"
    );
    server.shutdown().await;
}
