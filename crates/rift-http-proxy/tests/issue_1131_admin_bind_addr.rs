//! Issue #1131: the rule "`--local-only` pins loopback, otherwise `--host`, on `--port`" was
//! private to `server.rs`, so an embedder that composes its own admin listener on top of
//! `ServerBuilder` (the composition #807 opened the bootstrap seam for) had to copy it — and a
//! copy can disagree with `check_admin_exposure` across binaries in exactly the way the rule's own
//! doc comment guards against within one.
//!
//! These tests pin the promoted seam `server::admin_bind_addr`. The exposure classifier it feeds
//! is pinned separately in `issue_863_admin_exposure.rs`.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder, admin_bind_addr};
use std::net::{SocketAddr, TcpListener};

fn cli(argv: &[&str]) -> Cli {
    Cli::parse_from(argv)
}

/// Reserve a port, then release it, so the caller can bind it deterministically.
fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    port
}

// AC3 + AC4: the documented default — every interface, the Mountebank admin port.
#[test]
fn the_default_cli_binds_all_interfaces_on_2525() {
    let addr = admin_bind_addr(&cli(&["rift"])).expect("the default CLI must resolve");
    assert_eq!(addr, "0.0.0.0:2525".parse::<SocketAddr>().expect("literal"));
}

// AC2: `--local-only` pins loopback and outranks an explicit `--host`. This is the half of the
// rule an embedder most needs, and the half a copy most easily gets wrong.
#[test]
fn local_only_pins_loopback_over_an_explicit_host() {
    let addr = admin_bind_addr(&cli(&[
        "rift",
        "--local-only",
        "--host",
        "0.0.0.0",
        "--port",
        "3000",
    ]))
    .expect("--local-only must resolve");
    assert_eq!(
        addr,
        "127.0.0.1:3000".parse::<SocketAddr>().expect("literal")
    );
}

// AC3 + AC4: without `--local-only`, `--host` and `--port` are used verbatim.
#[test]
fn an_explicit_host_and_port_are_used_verbatim() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "127.0.0.2", "--port", "4321"]))
        .expect("an explicit literal host must resolve");
    assert_eq!(
        addr,
        "127.0.0.2:4321".parse::<SocketAddr>().expect("literal")
    );
}

// Edge: `--port 0` is how an embedder asks for an ephemeral admin port; it must resolve, not be
// mistaken for "unset".
#[test]
fn port_zero_resolves_to_an_ephemeral_bind() {
    let addr = admin_bind_addr(&cli(&["rift", "--port", "0"])).expect("port 0 must resolve");
    assert_eq!(addr, "0.0.0.0:0".parse::<SocketAddr>().expect("literal"));
}

// AC5: a non-literal host (a DNS name) is refused, and the refusal names the flag an operator
// would fix rather than leaving them with a bare `invalid socket address syntax`.
#[test]
fn a_non_literal_host_is_refused_naming_the_flag() {
    let err = admin_bind_addr(&cli(&["rift", "--host", "localhost"]))
        .expect_err("a DNS name is not a literal socket address and must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("--host") && msg.contains("localhost"),
        "the refusal must name the flag and the offending value, got: {msg}"
    );
}

// Edge: `--local-only` never reads `--host`, so an unparseable one must not make the loopback
// bind fail. A naive implementation that parses first and applies the rule second breaks here.
#[test]
fn local_only_ignores_an_unparseable_host() {
    let addr = admin_bind_addr(&cli(&["rift", "--local-only", "--host", "not-a-host"]))
        .expect("--local-only must not read --host");
    assert_eq!(
        addr,
        "127.0.0.1:2525".parse::<SocketAddr>().expect("literal")
    );
}

// Edge: the bracketed IPv6 spelling resolves (the bare one is pinned in `issue_1137_*`).
#[test]
fn a_bracketed_ipv6_host_resolves() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "[::1]", "--port", "7654"]))
        .expect("a bracketed IPv6 literal must resolve");
    assert_eq!(addr, "[::1]:7654".parse::<SocketAddr>().expect("literal"));
}

// AC6 — the property the seam exists for: what `start()` actually binds is what the seam reports.
// A second definition of the rule inside `start()` would pass every test above and fail this one.
#[tokio::test]
async fn start_binds_exactly_the_address_the_seam_reports() {
    let port = free_port();
    let cli = cli(&[
        "rift",
        "--local-only",
        "--host",
        "0.0.0.0",
        "--port",
        &port.to_string(),
    ]);
    let expected = admin_bind_addr(&cli).expect("seam must resolve");
    assert_eq!(
        expected,
        SocketAddr::from(([127, 0, 0, 1], port)),
        "guard: --local-only must pin loopback for this check to mean anything"
    );

    let server = ServerBuilder::from_cli(cli).start().await.expect("start");
    assert_eq!(
        server.admin_addr(),
        expected,
        "start() must bind the address admin_bind_addr reports — one definition of the rule"
    );
}
