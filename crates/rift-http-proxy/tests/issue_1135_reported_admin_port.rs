//! Issue #1135: `GET /config` reported `options.port` from the bound listener, which #879 made
//! truthful for the CLI. For an embedder that puts its own public listener in front of the admin
//! API and binds the core to an ephemeral loopback port — the composition #807 opened the bootstrap
//! seam for — `local_addr.port()` is the *private* port, so `/config` told the operator the admin
//! plane was on `54321` while every client reached it on `2525`. Mountebank-compat clients that read
//! `options.port` to build URLs got an unreachable address.
//!
//! `with_reported_admin_port` is the port's version of `with_local_only` (issue #879), which exists
//! for exactly the same reason: the value must be *the operator's configuration*, not "what happened
//! to bind".

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

/// Bind the admin plane on an ephemeral loopback port, optionally overriding what `/config` reports,
/// and return `(actual_bound_port, reported_port)`.
async fn bound_and_reported(reported: Option<u16>) -> (u16, u16) {
    let manager = Arc::new(ImposterManager::new());
    let mut server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None);
    if let Some(port) = reported {
        server = server.with_reported_admin_port(port);
    }
    let running = server.bind().await.expect("admin plane binds");
    let actual = running.local_addr().port();

    let body: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{actual}/config"))
        .await
        .expect("admin API reachable")
        .json()
        .await
        .expect("json");
    let reported_port = body["options"]["port"]
        .as_u64()
        .expect("options.port is a number") as u16;

    running.shutdown().await;
    (actual, reported_port)
}

// The default is untouched, so #879's pin (the CLI reports the port it actually bound) still holds.
#[tokio::test]
async fn without_the_override_config_reports_the_bound_port() {
    let (actual, reported) = bound_and_reported(None).await;

    assert_eq!(
        reported, actual,
        "with no override, /config must keep reporting the bound port (issue #879)"
    );
}

// The issue's own pin: the embedder's public port is reported, not the core's private one.
#[tokio::test]
async fn the_override_is_what_config_reports() {
    let (actual, reported) = bound_and_reported(Some(2525)).await;

    assert_eq!(
        reported, 2525,
        "/config must report the port the operator configured, not the ephemeral bind"
    );
    assert_ne!(
        actual, 2525,
        "guard: the bind must be ephemeral for this test to mean anything"
    );
}

// An override equal to the bound port is not special-cased away — the value is reported because it
// was configured, not because it differs from the bind.
#[tokio::test]
async fn an_override_matching_the_bound_port_is_still_reported() {
    let manager = Arc::new(ImposterManager::new());
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let port = probe.local_addr().expect("addr").port();
    drop(probe);

    let running = AdminApiServer::new(
        format!("127.0.0.1:{port}").parse().expect("addr"),
        manager,
        None,
    )
    .with_reported_admin_port(port)
    .bind()
    .await
    .expect("admin plane binds");

    let body: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/config"))
        .await
        .expect("admin API reachable")
        .json()
        .await
        .expect("json");

    assert_eq!(
        body["options"]["port"].as_u64(),
        Some(u64::from(port)),
        "an override equal to the bind is still the reported value"
    );
    running.shutdown().await;
}

// Port 0 is a real configured value for an embedder that wants to say "ephemeral" out loud, and
// must not be mistaken for "no override set" — the trap an `Option`-less design would fall into.
#[tokio::test]
async fn an_override_of_zero_is_honoured_not_treated_as_unset() {
    let (actual, reported) = bound_and_reported(Some(0)).await;

    assert_eq!(
        reported, 0,
        "an explicit override of 0 must be reported, not read as 'unset'"
    );
    assert_ne!(actual, 0, "guard: the real bind resolves to a live port");
}
