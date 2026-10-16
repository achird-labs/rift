//! Issue #1514: an imposter with no `host` listens where Mountebank's Node does by default,
//! `[::]` dual-stack, so it answers on `127.0.0.1` and on `::1` (a `localhost` client resolving
//! `::1` first reaches it). An explicit `"0.0.0.0"` stays IPv4-only. A port an IPv6-only listener
//! holds is a conflict, never retried on IPv4, as in Mountebank.

use rift_mock_core::imposter::{ImposterConfig, ImposterError, ImposterManager};
use serde_json::json;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv6Addr, SocketAddr, TcpListener};

fn config(host: Option<&str>, port: Option<u16>) -> ImposterConfig {
    let mut value = json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "served" } }] }]
    });
    if let Some(host) = host {
        value["host"] = json!(host);
    }
    if let Some(port) = port {
        value["port"] = json!(port);
    }
    serde_json::from_value(value).expect("test imposter config")
}

/// Whether this host can listen on `::1` at all.
fn has_ipv6_loopback() -> bool {
    TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

async fn get(url: &str) -> Result<String, reqwest::Error> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");
    client.get(url).send().await?.text().await
}

#[tokio::test]
async fn a_host_less_imposter_answers_on_ipv4_and_ipv6_loopback() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(config(None, None))
        .await
        .expect("create");
    assert_eq!(
        get(&format!("http://127.0.0.1:{port}/")).await.expect("v4"),
        "served"
    );
    if has_ipv6_loopback() {
        assert_eq!(
            get(&format!("http://[::1]:{port}/")).await.expect("v6"),
            "served"
        );
    }
    assert_eq!(
        manager.get_imposter(port).expect("exists").config.host,
        None,
        "the default is never written into the config"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn an_explicit_ipv4_wildcard_stays_ipv4_only() {
    if !has_ipv6_loopback() {
        return;
    }
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(config(Some("0.0.0.0"), None))
        .await
        .expect("create");
    assert_eq!(
        get(&format!("http://127.0.0.1:{port}/")).await.expect("v4"),
        "served"
    );
    assert!(
        get(&format!("http://[::1]:{port}/")).await.is_err(),
        "::1 must be refused for an explicit 0.0.0.0"
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn a_port_an_ipv6_only_listener_holds_is_in_use_not_retried_on_ipv4() {
    let Ok(holder) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) else {
        return; // no IPv6 on this host
    };
    holder.set_only_v6(true).expect("IPV6_V6ONLY");
    let any = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
    if holder.bind(&any.into()).is_err() {
        return;
    }
    holder.listen(4).expect("listen");
    let port = holder
        .local_addr()
        .expect("local addr")
        .as_socket()
        .expect("inet")
        .port();

    let manager = ImposterManager::new();
    let error = manager
        .create_imposter(config(None, Some(port)))
        .await
        .expect_err("the port is in use on IPv6");
    let ImposterError::BindError(failed, cause) = &error else {
        panic!("expected a BindError, got {error:?}");
    };
    assert_eq!(*failed, port);
    let io = cause.downcast_ref::<std::io::Error>().expect("an io error");
    assert_eq!(io.kind(), std::io::ErrorKind::AddrInUse);
    drop(holder);
}
