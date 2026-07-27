//! Issue #863: the admin plane binds `0.0.0.0` by default (`--host`, env `MB_HOST`), so a bare
//! keyless `rift` already exposes the full control plane — which can create imposters and drive the
//! TLS intercept proxy — on every interface with no authentication.
//!
//! Maintainer decision (option A): **warn by default, refuse only on opt-in**. `--host 0.0.0.0` is
//! the documented default and containers require it, so a hard refusal would break the no-argument
//! invocation, every quickstart and every CI sandbox. `--require-admin-auth` /
//! `RIFT_REQUIRE_ADMIN_AUTH` is the opt-in gate for fleets that want fail-closed.
//!
//! These tests pin the CLI door. The unit matrix over the classifier lives beside
//! `check_admin_exposure` in `admin_api/server.rs`; the C-ABI door is covered in
//! `rift-ffi/tests/round_trip.rs`.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::net::{SocketAddr, TcpListener};

/// Reserve a port, then release it, so the caller can assert whether something later bound it.
/// Binding `0.0.0.0` deliberately mirrors the metrics listener's own hardcoded bind address.
///
/// The gap between releasing the probe and re-binding below is a TOCTOU window: another process
/// could claim the port meanwhile. Nothing in-process races here (each `tests/*.rs` is its own
/// binary and this port is not shared), so this is only worth revisiting if it is ever seen to
/// flake in CI.
fn free_port() -> u16 {
    let probe = TcpListener::bind("0.0.0.0:0").expect("probe bind");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    port
}

// AC7: under `--require-admin-auth`, a non-loopback bind with no key must fail without leaving
// anything bound. The metrics listener is the discriminator: it binds early in `start()` and its
// handle moves the listener into a spawned task with no `Drop` that stops it, so an error raised
// after it would leave the port held with no way to reclaim it.
//
// Precisely: this pins the observable guarantee ("nothing is left bound on refusal"), not source
// ordering. Moving the check below the metrics bind fails this test — but moving it below *and*
// adding the explicit `metrics.shutdown().await` unwind that the front-door error path already
// uses would pass it. That is the intended contract either way; do not cite this test as proof of
// where the check sits.
#[tokio::test]
async fn require_admin_auth_refuses_a_keyless_non_loopback_bind_before_anything_binds() {
    let metrics_port = free_port();
    let cli = Cli::parse_from([
        "rift",
        "--host",
        "0.0.0.0",
        "--port",
        "0",
        "--metrics-port",
        &metrics_port.to_string(),
        "--require-admin-auth",
    ]);

    let err = ServerBuilder::from_cli(cli)
        .start()
        .await
        .err()
        .expect("--require-admin-auth must refuse a keyless non-loopback admin bind");
    let msg = err.to_string();
    assert!(
        msg.contains("--api-key"),
        "the refusal must name the flag an operator would fix, got: {msg}"
    );

    let addr = SocketAddr::from(([0, 0, 0, 0], metrics_port));
    TcpListener::bind(addr).unwrap_or_else(|e| {
        panic!(
            "the refusal must happen before the metrics listener binds, but port {metrics_port} \
             is still held: {e}"
        )
    });
}

// AC8: `--local-only` resolves the admin host to loopback, so strict mode is satisfied without a
// key — the escape hatch the warning points operators at must actually work under the strict flag.
#[tokio::test]
async fn require_admin_auth_accepts_local_only_without_a_key() {
    let cli = Cli::parse_from([
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--require-admin-auth",
    ]);
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("--local-only satisfies --require-admin-auth with no key");
    assert!(
        server.admin_addr().ip().is_loopback(),
        "--local-only must resolve the admin bind to loopback"
    );
    server.shutdown().await;
}

// Issue #880 (found while reviewing this change): `--local-only` must reach the metrics listener
// too. It used to hardcode `0.0.0.0`, so a flag documented as "only accept connections from
// localhost" left `/metrics` reachable on every interface.
#[tokio::test]
async fn local_only_restricts_the_metrics_listener_too() {
    let cli = Cli::parse_from(["rift", "--local-only", "--port", "0", "--metrics-port", "0"]);
    let server = ServerBuilder::from_cli(cli).start().await.expect("start");

    let metrics = server
        .metrics_addr()
        .expect("metrics listener bound on an ephemeral port");
    assert!(
        metrics.ip().is_loopback(),
        "--local-only must bind /metrics to loopback, got {metrics}"
    );

    server.shutdown().await;
}

// The converse: without `--local-only`, metrics keeps binding every interface. `--host` alone must
// NOT relocate it — that would move the listener for anyone who binds the admin plane to a specific
// interface, which is a separate decision from the one above.
#[tokio::test]
async fn without_local_only_metrics_still_binds_every_interface() {
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

    let metrics = server.metrics_addr().expect("metrics listener bound");
    assert!(
        metrics.ip().is_unspecified(),
        "--host must not relocate the metrics listener, got {metrics}"
    );

    server.shutdown().await;
}

// AC3 at the CLI door: strict mode gates on *authentication*, not on the address. A real key makes
// `0.0.0.0` acceptable — otherwise the flag would be an unavoidable `--local-only`, which is not
// what it means.
#[tokio::test]
async fn require_admin_auth_accepts_a_non_loopback_bind_with_a_key() {
    let cli = Cli::parse_from([
        "rift",
        "--host",
        "0.0.0.0",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--require-admin-auth",
        "--api-key",
        "s3cr3t",
    ]);
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("a real key satisfies --require-admin-auth on any address");
    server.shutdown().await;
}

// AC9: the compatibility guarantee. The default is a *warning*, never a refusal — a regression here
// breaks the no-argument invocation, every keyless quickstart and every existing container image.
#[tokio::test]
async fn the_default_keyless_non_loopback_bind_still_starts() {
    let cli = Cli::parse_from(["rift", "--port", "0", "--metrics-port", "0"]);
    assert_eq!(cli.host, "0.0.0.0", "the default --host must stay 0.0.0.0");
    assert!(
        !cli.require_admin_auth,
        "strict mode must be opt-in, never the default"
    );

    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("a keyless bare start must keep working; the default posture is a warning");
    server.shutdown().await;
}

// AC6: the flag is reachable by its env var too, so a fleet can set it once in a base image
// instead of editing every invocation.
//
// Asserted against clap's declared metadata rather than by setting the variable and re-parsing:
// the process environment is shared by every test in this binary, and mutating it mid-run flipped
// `the_default_keyless_non_loopback_bind_still_starts` above into the strict mode it exists to
// prove is NOT the default.
#[test]
fn require_admin_auth_is_reachable_by_env() {
    use clap::CommandFactory;

    let command = Cli::command();
    let arg = command
        .get_arguments()
        .find(|a| a.get_id() == "require_admin_auth")
        .expect("--require-admin-auth is declared");
    assert_eq!(
        arg.get_env().and_then(|e| e.to_str()),
        Some("RIFT_REQUIRE_ADMIN_AUTH"),
        "the flag must be settable from the environment"
    );
}
