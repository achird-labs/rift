//! Criterion bench for `ImposterManager::apply_config` re-applies (issue #1254).
//!
//! An embedder that reloads declaratively (`POST /admin/reload`, rift-cluster's per-write apply)
//! re-applies a set that is mostly unchanged. Before #1254 that cost a serialize-and-hash of every
//! live and desired stub plus a rebuilt match index per imposter, so an identical re-apply was
//! several times slower than creating the set from nothing. Cases, at the same set size:
//!
//! - `cold`: apply the set to an empty manager (binds every listener).
//! - `identical`: re-apply a freshly parsed copy of the running set.
//! - `one_stub_changed`: re-apply with one stub of one imposter changed.
//!
//! Fixtures use multi-key `equals` predicates and response headers, the shape whose keys were
//! unstable before #1256, so the identical case measures a true no-op, not wholesale replaces.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::json;

const IMPOSTERS: u16 = 200;
const STUBS_PER_IMPOSTER: usize = 5;
/// Reserved for this bench in `tests/test_port_uniqueness.rs`.
const PORT_BASE: u16 = 15100;

fn imposter(port: u16, changed: bool) -> ImposterConfig {
    let stubs: Vec<serde_json::Value> = (0..STUBS_PER_IMPOSTER)
        .map(|i| {
            let body = if changed && i == 0 {
                "changed".to_string()
            } else {
                format!("body-{port}-{i}")
            };
            json!({
                "predicates": [{ "equals": {
                    "method": "POST", "path": format!("/svc/{i}"),
                    "query": { "a": "1", "b": "2" }, "headers": { "X-Tenant": "t" }
                } }],
                "responses": [{ "is": {
                    "statusCode": 200,
                    "headers": { "Content-Type": "application/json", "X-A": "1", "X-B": "2", "X-C": "3" },
                    "body": { "id": i, "value": body }
                } }]
            })
        })
        .collect();
    serde_json::from_value(json!({ "protocol": "http", "port": port, "stubs": stubs }))
        .expect("bench imposter config")
}

/// A freshly parsed set, as a reload produces; `changed_port` gets one edited stub.
fn set(changed_port: Option<u16>) -> Vec<ImposterConfig> {
    (PORT_BASE..PORT_BASE + IMPOSTERS)
        .map(|port| imposter(port, Some(port) == changed_port))
        .collect()
}

fn bench_apply_config(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("apply_config");
    group.sample_size(10);

    // Times the apply only: building the set and tearing the listeners down are excluded.
    group.bench_function("cold", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                let configs = set(None);
                let manager = ImposterManager::new();
                let started = Instant::now();
                let report = runtime
                    .block_on(manager.apply_config(configs))
                    .expect("apply");
                total += started.elapsed();
                assert_eq!(report.created.len(), usize::from(IMPOSTERS));
                runtime.block_on(manager.delete_all());
            }
            total
        });
    });

    let manager = ImposterManager::new();
    runtime
        .block_on(manager.apply_config(set(None)))
        .expect("seed apply");

    group.bench_function("identical", |b| {
        b.iter_batched(
            || set(None),
            |configs| {
                let report = runtime
                    .block_on(manager.apply_config(configs))
                    .expect("apply");
                assert!(
                    report.replaced.is_empty()
                        && report.stub_patched.is_empty()
                        && report.created.is_empty(),
                    "an identical re-apply must change nothing: {report:?}"
                );
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // Alternate between two sets so every iteration is a real one-stub patch.
    let mut flip = false;
    group.bench_function("one_stub_changed", |b| {
        b.iter_batched(
            || {
                flip = !flip;
                set(flip.then_some(PORT_BASE))
            },
            |configs| {
                let report = runtime
                    .block_on(manager.apply_config(configs))
                    .expect("apply");
                assert_eq!(report.stub_patched.len(), 1, "{report:?}");
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
    runtime.block_on(manager.delete_all());
}

criterion_group!(benches, bench_apply_config);
criterion_main!(benches);
