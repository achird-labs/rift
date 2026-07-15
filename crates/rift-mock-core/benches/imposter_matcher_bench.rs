//! Criterion benches for the Mountebank imposter matcher hot path.
//!
//! These target the paths most imposter requests actually hit — `stub_matches` (predicate
//! evaluation) and `Imposter::find_matching_stub_with_client` (per-request stub scan) — so the
//! Tier 1/2 perf work (#286–#294) has a baseline to measure.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rift_mock_core::imposter::{Imposter, ImposterConfig, Predicate, stub_matches};
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
            .unwrap()
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
            .unwrap()
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
            .unwrap()
        })
    });

    group.finish();
}

/// Build an imposter whose `count` stubs each carry `predicate_for(i)` as their sole predicate.
fn imposter_with_stubs(
    count: usize,
    predicate_for: &dyn Fn(usize) -> serde_json::Value,
) -> Imposter {
    let stubs: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            json!({
                "predicates": [predicate_for(i)],
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
    Imposter::new(config).expect("test imposter")
}

/// Per-request stub scan, scaled 10 → 100 → 1000 stubs.
///
/// Three corpora, because the Stage-1 path-anchor index (issue #292) — not the stub count — is
/// what decides how much work a request actually does. `stub_index::classify` indexes a stub only
/// when a top-level predicate is a plain `equals`/`startsWith`/`contains` on `path`; every other
/// stub lands in the always-visited fallback bucket. Each request below targets the *last* stub,
/// so nothing short-circuits early:
///
/// * `indexed_exact` — every stub is `equals`-anchored, so `candidates()` resolves the request
///   path through a `HashMap` to a single candidate and Stage 2 evaluates one predicate. Flat in
///   stub count: this is the well-indexed common shape, and it is the *best* case, not the worst.
/// * `unindexed_fallback` — every stub matches on a path *regex*, which `path_anchor` will not
///   index, so all `count` stubs sit in fallback and each is fully predicate-evaluated. This is
///   the real O(stubs) worst case.
/// * `prefix_anchored` — every stub is `startsWith`-anchored. Those *are* indexed, but
///   `candidates()` walks each distinct prefix bucket linearly, so the prefilter itself scales
///   with the number of distinct prefixes even though exactly one stub matches.
fn bench_find_matching_stub(c: &mut Criterion) {
    let headers: HashMap<String, String> = HashMap::new();

    let mut scan = |group_name: &str,
                    predicate_for: &dyn Fn(usize) -> serde_json::Value,
                    request_path: &dyn Fn(usize) -> String| {
        let mut group = c.benchmark_group(group_name);
        for count in [10usize, 100, 1000] {
            let imposter = imposter_with_stubs(count, predicate_for);
            let path = request_path(count);
            // A fixture that stops matching still scans every stub, so it would keep producing
            // plausible numbers while silently measuring the no-match path instead. Pin it.
            assert!(
                imposter
                    .find_matching_stub_with_client("GET", &path, &headers, None, None, None, None)
                    .expect("matching must not error")
                    .is_some(),
                "{group_name}/{count}: fixture no longer matches its target stub",
            );
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
    };

    scan(
        "find_matching_stub_indexed_exact",
        &|i| json!({ "equals": { "path": format!("/api/endpoint{i}") } }),
        &|count| format!("/api/endpoint{}", count - 1),
    );
    scan(
        "find_matching_stub_unindexed_fallback",
        &|i| json!({ "matches": { "path": format!("^/api/endpoint{i}$") } }),
        &|count| format!("/api/endpoint{}", count - 1),
    );
    scan(
        "find_matching_stub_prefix_anchored",
        &|i| json!({ "startsWith": { "path": format!("/api/endpoint{i}/") } }),
        &|count| format!("/api/endpoint{}/detail", count - 1),
    );
}

criterion_group!(benches, bench_stub_matches, bench_find_matching_stub);
criterion_main!(benches);
