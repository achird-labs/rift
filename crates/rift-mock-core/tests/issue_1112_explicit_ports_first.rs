//! Issue #1112: `apply_config` created imposters in the order the set listed them, and an
//! auto-assigned port is the lowest free one from 49152. So a port-less imposter listed before an
//! explicit one could take the explicit port first. The explicit config then found that imposter on
//! "its" port and replaced or patched it, and the set applied with one imposter fewer and no error.
//!
//! Lives in its own test binary on purpose: the collision needs the port the next auto-assign will
//! pick to stay free between learning it and applying the set, and the in-crate suites run other
//! auto-assigning tests in parallel. No port is hardcoded, because the auto-assign floor may not be
//! (`test_port_uniqueness.rs`).

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::json;

fn config(value: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(value).expect("test imposter config")
}

#[tokio::test]
async fn an_auto_assigned_port_never_takes_an_explicit_port_from_the_same_set() {
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

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(
        report.replaced.is_empty() && report.stub_patched.is_empty(),
        "nothing existed to replace or patch: {report:?}"
    );
    assert_eq!(manager.count(), 2, "both imposters exist: {report:?}");
    let mut created = report.created.clone();
    created.sort_unstable();
    created.dedup();
    assert_eq!(created.len(), 2, "two distinct ports: {report:?}");
    assert!(created.contains(&taken), "{report:?}");
    assert_eq!(
        manager
            .get_imposter(taken)
            .expect("the explicit port is served")
            .config
            .name
            .as_deref(),
        Some("explicit"),
        "the explicit port belongs to the explicit config"
    );

    manager.delete_all().await;
}
