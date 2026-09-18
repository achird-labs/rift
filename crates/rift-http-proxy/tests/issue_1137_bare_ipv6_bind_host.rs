//! Issue #1137: `--host ::1` could never bind. Every bind door built its address by concatenating
//! `"{host}:{port}"` and parsing the result, and `"::1:2525"` reads the port as one more hextet.
//! The bracketed `[::1]` worked only by accident of that concatenation.
//!
//! These tests pin the admin plane and intercept listener doors; the shared parse rule is pinned in
//! `rift-mock-core`'s `proxy::network` tests, the imposter door in `imposter::manager`'s.

use clap::Parser;
use rift_http_proxy::intercept_control::{
    InterceptControl, InterceptStartError, InterceptStartOptions,
};
use rift_http_proxy::server::{Cli, ServerBuilder, admin_bind_addr, intercept_flag_options};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

fn cli(argv: &[&str]) -> Cli {
    Cli::parse_from(argv)
}

/// Tests that *bind* `::1` need a v6 loopback; CI has one, some containers do not.
fn has_v6_loopback() -> bool {
    let ok = std::net::TcpListener::bind("[::1]:0").is_ok();
    if !ok {
        eprintln!("skipping: this host has no IPv6 loopback");
    }
    ok
}

#[test]
fn a_bare_ipv6_host_resolves() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "::1", "--port", "7654"]))
        .expect("a bare IPv6 literal must resolve");
    assert_eq!(addr, "[::1]:7654".parse::<SocketAddr>().expect("literal"));
}

// `::` is the IPv6 spelling of "every interface" — the v6 counterpart of the `0.0.0.0` default.
#[test]
fn the_bare_ipv6_wildcard_resolves_on_the_default_port() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "::"])).expect("`::` must resolve");
    assert_eq!(addr, "[::]:2525".parse::<SocketAddr>().expect("literal"));
}

#[test]
fn a_v4_mapped_ipv6_host_resolves() {
    let addr = admin_bind_addr(&cli(&[
        "rift",
        "--host",
        "::ffff:127.0.0.1",
        "--port",
        "80",
    ]))
    .expect("a v4-mapped IPv6 literal must resolve");
    assert_eq!(
        addr,
        "[::ffff:127.0.0.1]:80"
            .parse::<SocketAddr>()
            .expect("literal")
    );
}

// A numeric scope id bound before #1137 (the old `SocketAddr` parse read it) and must still.
#[test]
fn a_scoped_link_local_host_keeps_its_scope() {
    let addr = admin_bind_addr(&cli(&["rift", "--host", "[fe80::1%2]", "--port", "80"]))
        .expect("a scoped IPv6 literal must resolve");
    let SocketAddr::V6(v6) = addr else {
        panic!("expected an IPv6 address, got {addr}");
    };
    assert_eq!(v6.scope_id(), 2);
    assert_eq!(v6.port(), 80);
}

// Brackets mean IPv6; an IPv4 address inside them is not an address, and the refusal must still
// name the flag and the value.
#[test]
fn a_bracketed_ipv4_host_is_refused_naming_the_flag() {
    let err = admin_bind_addr(&cli(&["rift", "--host", "[1.2.3.4]"]))
        .expect_err("`[1.2.3.4]` is not an IP literal");
    let msg = err.to_string();
    assert!(
        msg.contains("--host") && msg.contains("[1.2.3.4]"),
        "the refusal must name the flag and the offending value, got: {msg}"
    );
}

// The pre-fix refusal told an operator who wrote `::1` that it was "not a literal address". The
// wording must not blame an input the fix now accepts, and must name both IPv6 spellings.
#[test]
fn the_refusal_names_both_ipv6_spellings() {
    let msg = admin_bind_addr(&cli(&["rift", "--host", "localhost"]))
        .expect_err("a DNS name is refused")
        .to_string();
    assert!(
        msg.contains("`::1`") && msg.contains("`[::]`"),
        "the refusal must offer the bare and bracketed IPv6 spellings, got: {msg}"
    );
    assert!(
        !msg.contains("not a literal address"),
        "the pre-#1137 wording must be gone, got: {msg}"
    );
}

// What an operator on a v6-only host actually runs: the server starts, and binds `::1`.
#[tokio::test]
async fn the_server_starts_on_a_bare_ipv6_host() {
    if !has_v6_loopback() {
        return;
    }
    let server = ServerBuilder::from_cli(cli(&[
        "rift",
        "--host",
        "::1",
        "--port",
        "0",
        "--metrics-port",
        "0",
    ]))
    .start()
    .await
    .expect("`--host ::1` must start");
    assert_eq!(server.admin_addr().ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));
    server.shutdown().await;
}

// The early intercept exposure check re-parsed `host:intercept-port` with a bare `?`, so this
// combination died with an unattributed `invalid socket address syntax` before anything bound.
#[tokio::test]
async fn the_intercept_listener_starts_on_a_bare_ipv6_host_under_require_admin_auth() {
    if !has_v6_loopback() {
        return;
    }
    let server = ServerBuilder::from_cli(cli(&[
        "rift",
        "--host",
        "::1",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-port",
        "0",
        "--require-admin-auth",
        "--api-key",
        "k-1137-admin-secret",
        "--intercept-auth",
        "u:p-1137-intercept",
    ]))
    .start()
    .await
    .expect("`--host ::1 --intercept-port 0 --require-admin-auth` must start");
    server.shutdown().await;
}

#[tokio::test]
async fn intercept_start_binds_a_bare_ipv6_host() {
    if !has_v6_loopback() {
        return;
    }
    let control = InterceptControl::default();
    control
        .start(InterceptStartOptions {
            host: Some("::1".to_string()),
            port: Some(0),
            ..Default::default()
        })
        .await
        .expect("a bare IPv6 intercept host must bind");
    let addr = control.status().expect("the listener is running");
    assert_eq!(addr.ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_ne!(addr.port(), 0, "the OS assigned a real port");
    control.stop().await;
}

#[tokio::test]
async fn intercept_start_refuses_a_name_saying_what_it_takes() {
    let control = InterceptControl::default();
    let Err(err) = control
        .start(InterceptStartOptions {
            host: Some("localhost".to_string()),
            port: Some(0),
            ..Default::default()
        })
        .await
    else {
        panic!("a DNS name is not a bind literal");
    };
    match err {
        InterceptStartError::InvalidAddr(msg) => assert!(
            msg.contains("localhost") && msg.contains("IPv6"),
            "the refusal must name the value and what is accepted, got: {msg}"
        ),
        other => panic!("expected InvalidAddr, got {other:?}"),
    }
    assert!(control.status().is_none(), "a refused start binds nothing");
}

// Issue #1150: `--intercept-port` inherits the admin host as a *string*, and #1144 built that
// string with `admin_addr.ip().to_string()` — which cannot spell a scope id. The listener then
// re-parsed it as scope 0 and the bind failed, aborting startup. Asserted at the options level:
// binding a link-local interface is not something CI can rely on.
#[test]
fn the_intercept_flag_inherits_the_admin_scope_id() {
    let cli = cli(&[
        "rift",
        "--host",
        "[fe80::1%2]",
        "--port",
        "2525",
        "--intercept-port",
        "8443",
    ]);
    let admin_addr = admin_bind_addr(&cli).expect("a scoped IPv6 literal must resolve");
    let options =
        intercept_flag_options(&cli, admin_addr, None).expect("--intercept-port yields options");
    assert_eq!(options.host.as_deref(), Some("fe80::1%2"));
    assert_eq!(options.port, Some(8443));
}

// An unscoped host must not grow a `%0` on the way through.
#[test]
fn the_intercept_flag_inherits_an_unscoped_host_verbatim() {
    let cli = cli(&[
        "rift",
        "--host",
        "::1",
        "--port",
        "2525",
        "--intercept-port",
        "8443",
    ]);
    let admin_addr = admin_bind_addr(&cli).expect("`::1` must resolve");
    let options =
        intercept_flag_options(&cli, admin_addr, None).expect("--intercept-port yields options");
    assert_eq!(options.host.as_deref(), Some("::1"));
}

// No `--intercept-port` means no flag-derived listener, scope id or not.
#[test]
fn no_intercept_flag_yields_no_options() {
    let cli = cli(&["rift", "--host", "[fe80::1%2]", "--port", "2525"]);
    let admin_addr = admin_bind_addr(&cli).expect("a scoped IPv6 literal must resolve");
    assert!(intercept_flag_options(&cli, admin_addr, None).is_none());
}
