//! Issue #1575: a DNS-name `--host` is resolved at startup and the admin plane binds its first
//! address, as Mountebank's Node `listen(port, host)` does and as imposters already do (#1137).
//! Before, the admin plane refused any `--host` that was not an IP literal.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder, admin_bind_addr};
use std::net::ToSocketAddrs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn cli(argv: &[&str]) -> Cli {
    Cli::parse_from(argv)
}

/// The address `localhost` resolves to first on this host: what the admin plane must bind.
fn first_localhost(port: u16) -> std::net::SocketAddr {
    ("localhost", port)
        .to_socket_addrs()
        .expect("localhost resolves")
        .next()
        .expect("localhost has an address")
}

#[test]
fn a_dns_host_resolves_to_its_first_address() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "localhost", "--port", "3000"]))
        .expect("`--host localhost` must resolve");
    assert_eq!(addr, first_localhost(3000));
    assert!(addr.ip().is_loopback(), "localhost is loopback: {addr}");
}

#[test]
fn an_unresolvable_host_is_a_startup_error_naming_it() {
    let err = admin_bind_addr(&cli(&["rift", "--host", "no-such-host.invalid"]))
        .expect_err("a name that does not resolve has no address to bind");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("--host") && msg.contains("no-such-host.invalid"),
        "the error must name the flag and the host, got: {msg}"
    );
}

#[tokio::test]
async fn an_unresolvable_host_fails_startup() {
    let Err(err) = ServerBuilder::from_cli(cli(&[
        "rift",
        "--host",
        "no-such-host.invalid",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    else {
        panic!("`--host no-such-host.invalid` must not start");
    };
    assert!(
        format!("{err:#}").contains("no-such-host.invalid"),
        "{err:#}"
    );
}

// The server starts on `--host localhost`, binds loopback, and answers there.
#[tokio::test]
async fn the_server_starts_on_localhost_and_answers_on_loopback() {
    let server = ServerBuilder::from_cli(cli(&[
        "rift",
        "--host",
        "localhost",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("`--host localhost` must start");
    let addr = server.admin_addr();
    assert_eq!(addr.ip(), first_localhost(0).ip());

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the admin plane answers on its loopback address");
    stream
        .write_all(b"GET /imposters HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read the response");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    server.shutdown().await;
}

// The exposure check judges the resolved address (#863): `localhost` is loopback, so
// `--require-admin-auth` with no key starts rather than refusing an unresolved name.
#[tokio::test]
async fn require_admin_auth_judges_the_resolved_loopback_address() {
    let server = ServerBuilder::from_cli(cli(&[
        "rift",
        "--host",
        "localhost",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--require-admin-auth",
    ]))
    .start()
    .await
    .expect("a name resolving to loopback is not exposed");
    server.shutdown().await;
}
