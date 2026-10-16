//! Issue #1501: an auto-assigned port is picked by the kernel, as Mountebank's `listen(0)` picks it,
//! and the listener that learned it serves it. The old scan from 49152 probed each port with a
//! `SO_REUSEADDR` bind, which macOS lets succeed on a port another process holds on `127.0.0.1`;
//! that process then kept every loopback connection.
//!
//! Its own test binary: the loopback case learns the port the old scan would pick next, which only
//! stays meaningful while nothing else in the process auto-assigns. Ports come from `:0` binds only
//! (`test_port_uniqueness.rs`).

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::json;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashSet;
use std::net::{Ipv6Addr, SocketAddr, TcpListener};

fn port_less(body: &str) -> ImposterConfig {
    serde_json::from_value(json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": body } }] }]
    }))
    .expect("test imposter config")
}

/// A `SO_REUSEADDR` listener on `127.0.0.1:port`, as an unrelated local process would hold it.
fn loopback_squatter(port: u16) -> TcpListener {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("socket");
    socket.set_reuse_address(true).expect("SO_REUSEADDR");
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    socket
        .bind(&addr.into())
        .expect("bind the loopback squatter");
    socket.listen(16).expect("listen");
    socket.into()
}

/// An `IPV6_V6ONLY` listener on `[::]:0`, or `None` on a host without IPv6.
fn ipv6_only_listener() -> Option<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).ok()?;
    socket.set_only_v6(true).ok()?;
    let addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
    socket.bind(&addr.into()).ok()?;
    socket.listen(16).ok()?;
    Some(socket.into())
}

async fn body_at(port: u16) -> String {
    reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .expect("the imposter answers on 127.0.0.1")
        .text()
        .await
        .expect("body")
}

/// One test, run in sequence: the loopback case needs no other auto-assign in the process while
/// it learns and squats the next port, and test functions in one binary run concurrently.
#[tokio::test]
async fn an_auto_assigned_port_is_never_one_another_listener_holds() {
    a_loopback_listener_s_port_is_never_handed_out().await;
    an_ipv6_only_listener_s_port_is_never_handed_out().await;
}

async fn a_loopback_listener_s_port_is_never_handed_out() {
    let manager = ImposterManager::new();
    // The port the old scan would hand out next: create, read, delete.
    let learned = manager
        .create_imposter(port_less("learn"))
        .await
        .expect("learning create");
    manager.delete_imposter(learned).await.expect("delete");
    let _squatter = loopback_squatter(learned);

    let port = manager
        .create_imposter(port_less("from-imposter"))
        .await
        .expect("port-less create");

    assert_ne!(
        port, learned,
        "the port a loopback listener holds was handed out"
    );
    assert_eq!(body_at(port).await, "from-imposter");
    manager.delete_all().await;
}

async fn an_ipv6_only_listener_s_port_is_never_handed_out() {
    // Within the default 256-descriptor soft limit, with room for the imposters.
    let mut held = Vec::new();
    for _ in 0..120 {
        match ipv6_only_listener() {
            Some(listener) => held.push(listener),
            None if held.is_empty() => return, // no IPv6 on this host
            None => break,
        }
    }
    let held_ports: HashSet<u16> = held
        .iter()
        .map(|l| l.local_addr().expect("local addr").port())
        .collect();

    let manager = ImposterManager::new();
    for i in 0..20 {
        let port = manager
            .create_imposter(port_less(&format!("imposter-{i}")))
            .await
            .expect("port-less create");
        assert!(
            !held_ports.contains(&port),
            "port {port} is held by an IPv6-only listener: `localhost` clients would reach it"
        );
    }
    manager.delete_all().await;
}
