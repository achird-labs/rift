//! Issue #1112: `apply_config` created imposters in the order the set listed them, and an
//! auto-assigned port was the lowest free one from 49152. So a port-less imposter listed before an
//! explicit one could take the explicit port first. Since #1501 the kernel picks auto-assigned
//! ports, which makes the collision rare rather than certain, but a kernel can still reissue a
//! just-freed port, so the explicit-first ordering and this test stay. The explicit config then found that imposter on
//! "its" port and replaced or patched it, and the set applied with one imposter fewer and no error.
//!
//! Lives in its own test binary on purpose: the collision needs the port the next auto-assign will
//! pick to stay free between learning it and applying the set, and the in-crate suites run other
//! auto-assigning tests in parallel. No port is hardcoded, because the auto-assign floor may not be
//! (`test_port_uniqueness.rs`).

use rift_mock_core::imposter::{
    EventContext, ImposterConfig, ImposterError, ImposterEvent, ImposterEventListener,
    ImposterManager,
};
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

    // A port auto-assign just handed out: the old lowest-free scan handed it out again next, the
    // kernel (#1501) rarely does; the ordering test below does not depend on it.
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

/// Records the ports in the order `Created` fires.
#[derive(Default)]
struct CreationOrder(std::sync::Mutex<Vec<u16>>);

impl ImposterEventListener for CreationOrder {
    fn on_event(&self, event: &ImposterEvent, _ctx: &EventContext) {
        if let ImposterEvent::Created(port) = event {
            self.0.lock().expect("lock").push(*port);
        }
    }
}

/// The ordering itself, deterministically: since #1501 the kernel picks an auto-assigned port, so
/// the collision above is rare rather than certain, and only the creation order tells whether the
/// explicit config went first.
#[tokio::test]
async fn an_explicit_config_is_created_before_a_port_less_one_listed_ahead_of_it() {
    let explicit = std::net::TcpListener::bind("0.0.0.0:0")
        .and_then(|l| l.local_addr())
        .expect("a free port")
        .port();
    let order = std::sync::Arc::new(CreationOrder::default());
    let manager = ImposterManager::new().with_event_listener(order.clone());

    let report = manager
        .apply_config(vec![
            config(json!({"protocol": "http", "name": "auto", "stubs": []})),
            config(json!({"protocol": "http", "port": explicit, "name": "explicit", "stubs": []})),
        ])
        .await
        .expect("apply");
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    let created = order.0.lock().expect("lock").clone();
    assert_eq!(created.len(), 2, "{created:?}");
    assert_eq!(
        created[0], explicit,
        "the explicit config is created first: {created:?}"
    );
    manager.delete_all().await;
}
