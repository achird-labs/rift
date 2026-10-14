//! Issue #1112: `--configfile` startup created imposters in file order, and an auto-assigned port is
//! the lowest free one from 49152. A port-less imposter listed first could take the port the next
//! explicit imposter names, and that one then failed with `PortInUse` and was skipped.
//!
//! Its own test binary, because the test learns the next auto-assigned port and needs it to stay
//! free until the file loads; a shared binary runs other auto-assigning tests in parallel.

use clap::Parser;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::sync::Arc;

/// What one attempt found on the learned port.
enum Attempt {
    /// The explicit imposter holds it and both were created: the property holds.
    Held,
    /// The explicit imposter is missing. Another process held the port as the file loaded, so the
    /// explicit create failed to bind, and the port-less imposter may have taken it once it was
    /// free again (#1420). The regression this test exists for looks the same, but it is the
    /// engine's own ordering and recurs on every attempt.
    Missing,
}

/// Retried on [`Attempt::Missing`]: an outside collision is chance, the regression is not, so it
/// fails all five attempts. A process-wide lock cannot help, because the colliding auto-assign
/// runs in another test binary, and the bind error only reaches the log.
#[tokio::test]
async fn configfile_startup_creates_explicit_ports_before_auto_assigning() {
    for _ in 0..5 {
        if let Attempt::Held = attempt().await {
            return;
        }
    }
    panic!(
        "the explicit imposter was missing five times in a row: the port-less one took its port"
    );
}

async fn attempt() -> Attempt {
    let manager = Arc::new(ImposterManager::new());
    let learn: ImposterConfig =
        serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
            .expect("config");
    let taken = manager
        .create_imposter(learn)
        .await
        .expect("learn the next auto-assigned port");
    manager.delete_all().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(
        &path,
        serde_json::json!({"imposters": [
            {"protocol": "http", "name": "auto", "stubs": []},
            {"port": taken, "protocol": "http", "name": "explicit", "stubs": []},
        ]})
        .to_string(),
    )
    .expect("write config");

    let cli = Cli::try_parse_from([
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--configfile",
        path.to_str().expect("utf8 path"),
    ])
    .expect("cli parse");
    let running = ServerBuilder::from_cli(cli)
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("the config file loads");

    let on_taken = manager
        .get_imposter(taken)
        .ok()
        .map(|imposter| imposter.config.name.clone());
    let outcome = if on_taken != Some(Some("explicit".to_string())) {
        Attempt::Missing
    } else {
        assert_eq!(manager.count(), 2, "both imposters are created");
        Attempt::Held
    };

    running.shutdown().await;
    manager.delete_all().await;
    outcome
}
