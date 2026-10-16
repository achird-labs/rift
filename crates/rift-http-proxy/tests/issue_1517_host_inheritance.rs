//! Issue #1517: Mountebank hands an imposter created without a `host` the server's `--host`
//! (`imposter.js:194-195`), so `mb --host 127.0.0.1` keeps imposters on loopback as well as its
//! admin API. Rift bound host-less imposters on every interface whatever `--host` said, so a rig that
//! confined the admin API to loopback still exposed every imposter.
//!
//! These tests pin the inheritance at each door (admin create with an explicit and an auto-assigned
//! port, `--configfile` startup, `--datadir` restart), that an imposter's own `host` wins, that the
//! inherited host is never printed or stored, and that a manager injected through
//! `ServerBuilder::manager` is left alone (rift-cluster injects one and pins `--host` to loopback for
//! its own admin). Every port is OS-assigned.
//!
//! "Confined" is observed from outside: the imposter must refuse on `[::1]`, which the dual-stack
//! `[::]` a host-less imposter otherwise binds answers, and on a non-loopback address of this host
//! when it has one. Each half is skipped where the host lacks it.

use clap::Parser;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use serial_test::serial;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

const V4_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const V6_LOOPBACK: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

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

fn accepts(ip: IpAddr, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2)).is_ok()
}

/// Answers on `127.0.0.1` only: not on `[::1]`, nor on this host's off-host address.
fn assert_loopback_v4_only(port: u16) {
    assert!(accepts(V4_LOOPBACK, port), "127.0.0.1:{port} must answer");
    if has_ipv6_loopback() {
        assert!(
            !accepts(V6_LOOPBACK, port),
            "[::1]:{port} must refuse: the imposter inherited --host 127.0.0.1"
        );
    }
    if let Some(ip) = non_loopback_local_ip() {
        assert!(
            !accepts(ip, port),
            "{ip}:{port} must refuse: the imposter inherited --host 127.0.0.1"
        );
    }
}

/// Answers beyond `127.0.0.1`: on `[::1]` when this host has it, and on the off-host address.
fn assert_not_confined(port: u16) {
    assert!(accepts(V4_LOOPBACK, port), "127.0.0.1:{port} must answer");
    if has_ipv6_loopback() {
        assert!(
            accepts(V6_LOOPBACK, port),
            "[::1]:{port} must answer: a host-less imposter with no --host is dual-stack"
        );
    }
    if let Some(ip) = non_loopback_local_ip() {
        assert!(accepts(ip, port), "{ip}:{port} must answer");
    }
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

/// `POST /imposters` and the port it bound.
async fn create(server: &RunningServer, body: Value) -> u16 {
    let response = client()
        .post(format!(
            "http://127.0.0.1:{}/imposters",
            server.admin_addr().port()
        ))
        .json(&body)
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(response.status().as_u16(), 201, "create {body}");
    let created: Value = response.json().await.expect("json");
    let port = created["port"].as_u64().expect("port");
    u16::try_from(port).expect("port fits u16")
}

async fn get(server: &RunningServer, path: &str) -> Value {
    client()
        .get(format!(
            "http://127.0.0.1:{}{path}",
            server.admin_addr().port()
        ))
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path}: {e}"))
        .json()
        .await
        .expect("json")
}

/// Delete every imposter, then stop the server. `shutdown` stops only the server's own listeners;
/// the imposters live in its manager and would keep their ports for the rest of this binary.
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

fn host_less(port: Option<u16>) -> Value {
    let mut v = json!({"protocol": "http", "stubs": [{"responses": [{"is": {"body": "ok"}}]}]});
    if let Some(p) = port {
        v["port"] = json!(p);
    }
    v
}

/// A port no listener holds right now, on the dual-stack wildcard a host-less imposter would bind.
fn free_port() -> u16 {
    std::net::TcpListener::bind("[::]:0")
        .or_else(|_| std::net::TcpListener::bind("0.0.0.0:0"))
        .and_then(|l| l.local_addr())
        .expect("probe a free port")
        .port()
}

#[tokio::test]
#[serial]
async fn host_less_imposters_inherit_a_loopback_host() {
    let server = start(&["--host", "127.0.0.1"]).await;
    let explicit = create(&server, host_less(Some(free_port()))).await;
    let auto = create(&server, host_less(None)).await;
    assert_loopback_v4_only(explicit);
    assert_loopback_v4_only(auto);
    stop(&server).await;
}

#[tokio::test]
#[serial]
async fn an_imposter_host_wins_over_the_inherited_one() {
    let server = start(&["--host", "127.0.0.1"]).await;
    let mut body = host_less(None);
    body["host"] = json!("0.0.0.0");
    let port = create(&server, body).await;
    assert!(accepts(V4_LOOPBACK, port), "127.0.0.1:{port} must answer");
    if let Some(ip) = non_loopback_local_ip() {
        assert!(
            accepts(ip, port),
            "{ip}:{port} must answer: the imposter's own 0.0.0.0 wins over --host"
        );
    }
    stop(&server).await;
}

/// Mountebank prints only what was posted, so an inherited host never appears in the detail or the
/// replayable export, and therefore not in `mb save`.
#[tokio::test]
#[serial]
async fn the_inherited_host_is_never_printed() {
    let server = start(&["--host", "127.0.0.1"]).await;
    let port = create(&server, host_less(None)).await;
    for path in [
        format!("/imposters/{port}"),
        format!("/imposters/{port}?replayable=true"),
    ] {
        let printed = get(&server, &path).await;
        assert!(printed.get("host").is_none(), "{path} printed {printed}");
    }
    let all = get(&server, "/imposters?replayable=true").await;
    assert!(
        all["imposters"][0].get("host").is_none(),
        "replayable list printed {all}"
    );
    stop(&server).await;
}

/// The datadir stores the posted config, so a datadir imposter follows the `--host` of the server
/// that loads it rather than the one that created it.
#[tokio::test]
#[serial]
async fn a_datadir_imposter_follows_the_loading_servers_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let datadir = dir.path().to_str().expect("utf8 path");

    let first = start(&["--host", "127.0.0.1", "--datadir", datadir]).await;
    let port = create(&first, host_less(None)).await;
    assert_loopback_v4_only(port);
    let file = dir.path().join(format!("{port}.json"));
    let bytes = std::fs::read(&file).expect("read datadir file");
    let stored: Value = serde_json::from_slice(&bytes).expect("datadir file is json");
    assert!(stored.get("host").is_none(), "datadir stored {stored}");
    // Deleting frees the port and removes the file, so put the file back for the restart.
    stop(&first).await;
    std::fs::write(&file, &bytes).expect("restore datadir file");

    let second = start(&["--datadir", datadir]).await;
    assert_not_confined(port);
    stop(&second).await;
}

#[tokio::test]
#[serial]
async fn a_configfile_imposter_inherits_the_host_at_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(&path, json!({"imposters": [host_less(None)]}).to_string())
        .expect("write config");

    let server = start(&[
        "--host",
        "127.0.0.1",
        "--configfile",
        path.to_str().expect("utf8 path"),
    ])
    .await;
    let listed = get(&server, "/imposters").await;
    let port = listed["imposters"][0]["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or_else(|| panic!("one imposter listed: {listed}"));
    assert_loopback_v4_only(port);
    stop(&server).await;
}

/// An embedder injecting its own manager keeps its own bind policy: rift-cluster pins `--host` to
/// loopback for its core admin, and every clustered imposter would go dark if that leaked in.
#[tokio::test]
#[serial]
async fn an_injected_manager_does_not_inherit_the_host() {
    let manager = Arc::new(ImposterManager::new());
    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ])
    .expect("cli parse");
    let server = ServerBuilder::from_cli(cli)
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("start");

    let config: ImposterConfig = serde_json::from_value(host_less(None)).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    assert_not_confined(port);

    server.shutdown().await;
    manager.delete_all().await;
}
