//! Issue #1112: `apply_config` created imposters in the order the set listed them, and an
//! auto-assigned port is the lowest free one from 49152. So a port-less imposter listed before an
//! explicit one could take the explicit port first. The explicit config then found that imposter on
//! "its" port and replaced or patched it, and the set applied with one imposter fewer and no error.
//!
//! Lives in its own test binary on purpose: the collision needs the port the next auto-assign will
//! pick to stay free between learning it and applying the set, and the in-crate suites run other
//! auto-assigning tests in parallel. No port is hardcoded, because the auto-assign floor may not be
//! (`test_port_uniqueness.rs`).

use rift_mock_core::imposter::{ImposterConfig, ImposterError, ImposterManager};
use serde_json::json;

fn config(value: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(value).expect("test imposter config")
}

/// Retried only when another process held the learned port as the set applied (#1420): the
/// explicit config then fails with a socket-level `BindError`, which this engine cannot prevent,
/// and the port-less imposter may get the port once it is free again. The regression this test
/// exists for is the engine's own doing — the port-less imposter takes the port first and the
/// explicit config replaces or patches it — and is asserted on every attempt, never retried.
#[tokio::test]
async fn an_auto_assigned_port_never_takes_an_explicit_port_from_the_same_set() {
    for _ in 0..5 {
        if attempt().await {
            return;
        }
    }
    panic!("another process took the learned port five times in a row");
}

/// `true` when the property held; `false` when another process took the learned port.
async fn attempt() -> bool {
    let manager = ImposterManager::new();

    // The port the next auto-assign on this host will choose.
    let learned = manager
        .apply_config(vec![config(json!({"protocol": "http", "stubs": []}))])
        .await
        .expect("learning apply");
    let taken = learned.created[0];
    manager.delete_all().await;

    let report = manager
        .apply_config(vec![
            config(json!({"protocol": "http", "name": "auto", "stubs": []})),
            config(json!({"protocol": "http", "port": taken, "name": "explicit", "stubs": []})),
        ])
        .await
        .expect("a set with one port-less and one explicit imposter applies");

    assert!(
        report.replaced.is_empty() && report.stub_patched.is_empty(),
        "nothing existed to replace or patch: {report:?}"
    );
    let taken_outside = report
        .failed
        .iter()
        .any(|(port, error)| *port == taken && matches!(error, ImposterError::BindError(..)));
    if taken_outside {
        manager.delete_all().await;
        return false;
    }
    let on_taken = manager
        .get_imposter(taken)
        .ok()
        .map(|imposter| imposter.config.name.clone());

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(manager.count(), 2, "both imposters exist: {report:?}");
    let mut created = report.created.clone();
    created.sort_unstable();
    created.dedup();
    assert_eq!(created.len(), 2, "two distinct ports: {report:?}");
    assert!(created.contains(&taken), "{report:?}");
    assert_eq!(
        on_taken,
        Some(Some("explicit".to_string())),
        "the explicit port belongs to the explicit config"
    );

    manager.delete_all().await;
    true
}
