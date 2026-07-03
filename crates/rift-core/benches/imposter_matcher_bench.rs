//! Criterion benches for the Mountebank imposter matcher hot path.
//!
//! Complements `rift-http-proxy/benches/matcher_bench.rs` (which covers the proxy
//! `RuleIndex`/`CompiledRule` engine). These target the paths most imposter requests actually
//! hit — `stub_matches` (predicate evaluation) and `Imposter::find_matching_stub_with_client`
//! (per-request stub scan) — so the Tier 1/2 perf work (#286–#294) has a baseline to measure.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use hyper::HeaderMap;
use rift_core::imposter::{Imposter, ImposterConfig, Predicate, stub_matches};
use serde_json::json;

fn predicates_from(values: serde_json::Value) -> Vec<Predicate> {
    serde_json::from_value(values).expect("valid predicate fixtures")
}

/// Pure predicate evaluation, isolated from stub-scan overhead: regex `matches`, `deepEquals`
/// over a JSON body, and combined header + query matching.
fn bench_stub_matches(c: &mut Criterion) {
    let mut group = c.benchmark_group("stub_matches");
    let empty_headers: HashMap<String, String> = HashMap::new();

    let regex_preds = predicates_from(json!([
        { "matches": { "path": r"^/api/v\d+/users/\d+$" } }
    ]));
    group.bench_function("regex_matches", |b| {
        b.iter(|| {
            stub_matches(
                black_box(&regex_preds),
                "GET",
                black_box("/api/v2/users/4815"),
                None,
                &empty_headers,
                None,
                None,
                None,
                None,
                0,
            )
        })
    });

    let deep_preds = predicates_from(json!([
        { "deepEquals": { "body": { "user": { "id": 42, "name": "ada" }, "roles": ["admin", "ops"] } } }
    ]));
    let body = r#"{"roles":["admin","ops"],"user":{"name":"ada","id":42}}"#;
    group.bench_function("deep_equals_json_body", |b| {
        b.iter(|| {
            stub_matches(
                black_box(&deep_preds),
                "POST",
                "/orders",
                None,
                &empty_headers,
                black_box(Some(body)),
                None,
                None,
                None,
                0,
            )
        })
    });

    let header_query_preds = predicates_from(json!([
        { "equals": { "headers": { "X-Trace": "trace-123" } } },
        { "equals": { "query": { "page": "2" } } }
    ]));
    let mut headers = HashMap::new();
    headers.insert("X-Trace".to_string(), "trace-123".to_string());
    group.bench_function("header_and_query", |b| {
        b.iter(|| {
            stub_matches(
                black_box(&header_query_preds),
                "GET",
                "/list",
                black_box(Some("page=2&sort=asc")),
                &headers,
                None,
                None,
                None,
                None,
                0,
            )
        })
    });

    group.finish();
}

fn imposter_with_stubs(count: usize) -> Imposter {
    let stubs: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            json!({
                "predicates": [{ "equals": { "path": format!("/api/endpoint{i}") } }],
                "responses": [{ "is": { "statusCode": 200 } }]
            })
        })
        .collect();
    let config: ImposterConfig = serde_json::from_value(json!({
        "protocol": "http",
        "port": 20000,
        "stubs": stubs,
    }))
    .expect("valid imposter config");
    Imposter::new(config)
}

/// End-to-end per-request stub scan, scaled 10 → 100 → 1000 stubs. The request targets the last
/// stub so the whole list is scanned (worst case for the current O(n) matcher).
fn bench_find_matching_stub(c: &mut Criterion) {
    let mut group = c.benchmark_group("find_matching_stub");
    let headers = HeaderMap::new();

    for count in [10usize, 100, 1000] {
        let imposter = imposter_with_stubs(count);
        let path = format!("/api/endpoint{}", count - 1);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                imposter
                    .find_matching_stub_with_client(
                        "GET",
                        black_box(path.as_str()),
                        black_box(&headers),
                        None,
                        None,
                        None,
                        None,
                    )
                    .expect("matching must not error")
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_stub_matches, bench_find_matching_stub);
criterion_main!(benches);
