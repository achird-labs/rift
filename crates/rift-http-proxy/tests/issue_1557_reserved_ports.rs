//! Issue #1557: an imposter must not take a port this server's own listeners hold — the admin API,
//! `/metrics`, the front door or the intercept proxy.
//!
//! Imposters bind with `SO_REUSEADDR`. On macOS that lets a wildcard bind sit beside a listener on
//! a specific address (`--local-only`'s loopback admin, `--host 127.0.0.1` against the wildcard
//! metrics listener, a loopback `--front-door`), and the two then split the port's loopback
//! clients. Linux already refused each case with `EADDRINUSE`. Mountebank refuses its one
//! listener, the admin port, with a 400, and Rift now answers its existing 400 `already in use` on
//! every platform.
//!
//! Every port is OS-assigned. Each test checks that the original listener still answers on the
//! port afterwards, on `127.0.0.1` and, where it binds there too, `[::1]`.

use clap::Parser;
use rift_http_proxy::imposter::{ImposterConfig, ImposterError, ImposterManager};
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use serial_test::serial;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

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

fn admin_url(server: &RunningServer, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", server.admin_addr().port())
}

/// A host-less imposter on `port`: it binds `[::]` dual-stack, the bind that slipped past a
/// loopback listener on macOS.
fn imposter_on(port: u16) -> Value {
    json!({
        "protocol": "http",
        "port": port,
        "stubs": [{"responses": [{"is": {"body": "imposter"}}]}]
    })
}

/// `POST /imposters` on `port`: the status and body.
async fn post(server: &RunningServer, port: u16) -> (u16, String) {
    let response = client()
        .post(admin_url(server, "/imposters"))
        .json(&imposter_on(port))
        .send()
        .await
        .expect("POST /imposters");
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

/// `PUT /imposters` with one imposter on `port`: the status and body.
async fn put(server: &RunningServer, port: u16) -> (u16, String) {
    let response = client()
        .put(admin_url(server, "/imposters"))
        .json(&json!({"imposters": [imposter_on(port)]}))
        .send()
        .await
        .expect("PUT /imposters");
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

/// Both admin doors refuse `port` with the existing 400, naming it as in use, and no imposter is
/// left behind.
async fn assert_refused(server: &RunningServer, port: u16, what: &str) {
    for (door, (status, body)) in [
        ("POST", post(server, port).await),
        ("PUT", put(server, port).await),
    ] {
        assert_eq!(
            status, 400,
            "{door} /imposters on the {what} port {port} must be refused, got {status}: {body}"
        );
        assert!(
            body.contains("already in use"),
            "{door} on the {what} port {port}: {body}"
        );
    }
    let listed: Value = client()
        .get(admin_url(server, "/imposters"))
        .send()
        .await
        .expect("GET /imposters")
        .json()
        .await
        .expect("json");
    assert_eq!(
        listed["imposters"],
        json!([]),
        "a refused imposter on the {what} port must not exist"
    );
}

/// One `GET path` over a raw socket, returning whatever came back (empty when nothing answered).
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

/// Whether this host can bind `[::1]`; a v4-only host has no second loopback to check.
fn has_ipv6_loopback() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// Delete whatever a red run left, then stop the server.
async fn stop(server: &RunningServer) {
    let response = client()
        .delete(admin_url(server, "/imposters"))
        .send()
        .await
        .expect("DELETE /imposters");
    assert!(response.status().is_success(), "DELETE /imposters");
    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn local_only_refuses_every_server_port() {
    let server = start(&[
        "--local-only",
        "--front-door",
        "127.0.0.1:0",
        "--intercept-port",
        "0",
    ])
    .await;
    let admin = server.admin_addr().port();
    let metrics = server.metrics_addr().expect("metrics bound").port();
    let front_door = server.front_door_addr().expect("front door bound").port();
    let intercept = server.intercept_addr().expect("intercept bound").port();

    assert_refused(&server, admin, "admin").await;
    assert_refused(&server, metrics, "metrics").await;
    assert_refused(&server, front_door, "front door").await;
    assert_refused(&server, intercept, "intercept").await;

    // Each listener still owns its port: no imposter took a share of its clients.
    let mut loopbacks = vec![V4];
    if has_ipv6_loopback() {
        loopbacks.push(V6);
    }
    for ip in loopbacks {
        let reply = raw_get(ip, admin, "/imposters").await;
        assert!(
            reply.starts_with("HTTP/1.1 200") && reply.contains("imposters"),
            "admin on {ip}:{admin}: {reply}"
        );
        let reply = raw_get(ip, metrics, "/metrics").await;
        assert!(
            reply.starts_with("HTTP/1.1 200") && !reply.contains("imposter\r\n"),
            "metrics on {ip}:{metrics}: {reply}"
        );
    }
    let reply = raw_get(V4, front_door, "/").await;
    assert!(
        !reply.ends_with("imposter"),
        "front door on 127.0.0.1:{front_door} answered as the imposter: {reply}"
    );
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn host_loopback_refuses_the_wildcard_metrics_port() {
    // `--host` moves the admin plane and imposters to `127.0.0.1` but leaves `/metrics` on the
    // wildcard: on macOS the imposter's specific bind slipped in beside it and took `/metrics`.
    let server = start(&["--host", "127.0.0.1"]).await;
    let metrics = server.metrics_addr().expect("metrics bound").port();

    assert_refused(&server, metrics, "metrics").await;
    let reply = raw_get(V4, metrics, "/metrics").await;
    assert!(
        reply.starts_with("HTTP/1.1 200") && !reply.ends_with("imposter"),
        "metrics on 127.0.0.1:{metrics}: {reply}"
    );
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn a_loopback_front_door_port_is_refused() {
    let server = start(&["--front-door", "127.0.0.1:0"]).await;
    let front_door = server.front_door_addr().expect("front door bound").port();

    assert_refused(&server, front_door, "front door").await;
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn an_injected_manager_learns_the_server_ports_and_forgets_them_at_shutdown() {
    // rift-cluster hands `ServerBuilder` its own manager: the server's ports must be refused
    // through that manager too, including on its direct `create_imposter` door.
    let manager = Arc::new(ImposterManager::new());
    let cli = Cli::try_parse_from(["rift", "--local-only", "--port", "0", "--metrics-port", "0"])
        .expect("cli parse");
    let server = ServerBuilder::from_cli(cli)
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("start");
    let admin = server.admin_addr().port();
    let metrics = server.metrics_addr().expect("metrics bound").port();

    for port in [admin, metrics] {
        let config: ImposterConfig =
            serde_json::from_value(imposter_on(port)).expect("imposter config");
        let result = manager.create_imposter(config).await;
        assert!(
            matches!(result, Err(ImposterError::PortInUse(p)) if p == port),
            "the server port {port} must be refused through the injected manager: {result:?}"
        );
    }
    let _ = manager.delete_all().await;
    server.shutdown().await;

    // Released at shutdown: the manager outlives the server, and the port is free again.
    let config: ImposterConfig =
        serde_json::from_value(imposter_on(metrics)).expect("imposter config");
    let created = manager.create_imposter(config).await;
    assert!(
        matches!(created, Ok(p) if p == metrics),
        "after shutdown the metrics port {metrics} is no longer the server's: {created:?}"
    );
    let _ = manager.delete_all().await;
}

#[tokio::test]
#[serial]
async fn a_runtime_intercept_start_on_an_imposter_port_is_refused() {
    // The other direction: the intercept listener binds `127.0.0.1`, and on macOS it slipped in
    // beside a host-less imposter's `[::]` and took its IPv4 loopback clients.
    let server = start(&["--local-only"]).await;
    let response = client()
        .post(admin_url(&server, "/imposters"))
        .json(&json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"body": "imposter"}}]}]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status().as_u16(), 201, "create");
    let created: Value = response.json().await.expect("json");
    let port = u16::try_from(created["port"].as_u64().expect("port")).expect("port fits u16");

    let response = client()
        .post(admin_url(&server, "/intercept"))
        .json(&json!({"port": port}))
        .send()
        .await
        .expect("POST /intercept");
    let status = response.status().as_u16();
    let body = response.text().await.expect("body");
    assert_eq!(
        status, 400,
        "an intercept listener on the imposter's port {port} must be refused: {body}"
    );
    assert!(body.contains("already in use"), "{body}");
    assert!(
        server.intercept_addr().is_none(),
        "no intercept listener runs"
    );
    let reply = raw_get(V4, port, "/").await;
    assert!(
        reply.starts_with("HTTP/1.1 200") && reply.ends_with("imposter"),
        "the imposter still answers on 127.0.0.1:{port}: {reply}"
    );
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn a_startup_imposter_keeps_the_metrics_port() {
    // Startup imposters load before `/metrics` binds. One on the metrics port keeps it, and the
    // metrics bind fails without stopping the server, as it always did on Linux.
    let port = std::net::TcpListener::bind("[::]:0")
        .or_else(|_| std::net::TcpListener::bind("0.0.0.0:0"))
        .expect("probe a free port")
        .local_addr()
        .expect("probe addr")
        .port();
    let datadir = tempfile::tempdir().expect("datadir");
    std::fs::write(
        datadir.path().join(format!("{port}.json")),
        imposter_on(port).to_string(),
    )
    .expect("write the startup imposter");
    let port_arg = port.to_string();
    let datadir_arg = datadir.path().to_str().expect("utf-8 datadir").to_owned();
    let cli = Cli::try_parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        port_arg.as_str(),
        "--datadir",
        datadir_arg.as_str(),
    ])
    .expect("cli parse");
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");

    assert!(
        server.metrics_addr().is_none(),
        "the metrics server must not bind beside the imposter on {port}"
    );
    let reply = raw_get(V4, port, "/").await;
    assert!(
        reply.starts_with("HTTP/1.1 200") && reply.ends_with("imposter"),
        "the startup imposter owns 127.0.0.1:{port}: {reply}"
    );
    stop(&server).await;
}
