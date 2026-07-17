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
/// * `regex_anchored` — every stub matches on a path *regex*, targeting the last. Before #709
///   `path_anchor` would not index these, so all `count` stubs sat in fallback and each was fully
///   predicate-evaluated — the O(stubs) worst case. #709's regex dimension answers "which of these
///   `count` patterns match" in one automaton pass, so this is now the headline before/after.
/// * `prefix_anchored` — every stub is `startsWith`-anchored. Before #710 `candidates()` walked
///   each distinct prefix bucket linearly, so the prefilter itself scaled with the number of
///   distinct prefixes even though exactly one stub matched; the literal dimension's Aho-Corasick
///   pass replaces that walk.
/// * `literal_mixed` — a corpus mixing all three literal kinds (`startsWith`/`contains`/`endsWith`)
///   over one automaton, targeting the last stub. `endsWith` was not indexed at all before #710, so
///   a third of this corpus used to sit in the always-visited fallback.
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
        "find_matching_stub_regex_anchored",
        &|i| json!({ "matches": { "path": format!("^/api/endpoint{i}$") } }),
        &|count| format!("/api/endpoint{}", count - 1),
    );
    scan(
        "find_matching_stub_prefix_anchored",
        &|i| json!({ "startsWith": { "path": format!("/api/endpoint{i}/") } }),
        &|count| format!("/api/endpoint{}/detail", count - 1),
    );
    scan(
        "find_matching_stub_literal_mixed",
        &|i| match i % 3 {
            0 => json!({ "startsWith": { "path": format!("/api/pre{i}/") } }),
            1 => json!({ "contains": { "path": format!("~mid{i}~") } }),
            _ => json!({ "endsWith": { "path": format!("/end{i}") } }),
        },
        &|count| {
            let i = count - 1;
            match i % 3 {
                0 => format!("/api/pre{i}/detail"),
                1 => format!("/x~mid{i}~y"),
                _ => format!("/whatever/end{i}"),
            }
        },
    );
}

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"];

/// Issue #707: what the method dimension buys, isolated from the path index.
///
/// Every stub shares ONE path, so the path dimension resolves *all* of them as candidates and can
/// prune nothing — pre-#707 that meant a full predicate evaluation of every stub on every request,
/// because the index could not see the method at all. Stubs are distinguished by method (6-way) and
/// by a body, and the request targets the *last* stub, so nothing short-circuits early.
///
/// The method dimension collapses the candidate set to the ~1/6 of stubs sharing the request's
/// method, so this should scale at ~1/6 the per-stub work — and the collapse itself is asserted
/// exactly (not just timed) by `method_dimension_collapses_candidates` in `stub_index.rs`.
fn bench_method_dimension(c: &mut Criterion) {
    let headers: HashMap<String, String> = HashMap::new();
    let mut group = c.benchmark_group("find_matching_stub_method_disjoint");

    for count in [10usize, 100, 1000] {
        let imposter = imposter_with_stubs(count, &|i| {
            json!({ "equals": {
                "method": METHODS[i % METHODS.len()],
                "path": "/api/shared",
                "body": format!("req{i}"),
            }})
        });
        // Target the last stub: its method, its body.
        let last = count - 1;
        let method = METHODS[last % METHODS.len()];
        let body = format!("req{last}");

        // A fixture that stops matching would silently measure the no-match path instead. Pin it.
        assert!(
            imposter
                .find_matching_stub_with_client(
                    method,
                    "/api/shared",
                    &headers,
                    None,
                    Some(&body),
                    None,
                    None
                )
                .expect("matching must not error")
                .is_some(),
            "method_disjoint/{count}: fixture no longer matches its target stub",
        );

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                imposter
                    .find_matching_stub_with_client(
                        black_box(method),
                        black_box("/api/shared"),
                        black_box(&headers),
                        None,
                        black_box(Some(body.as_str())),
                        None,
                        None,
                    )
                    .expect("matching must not error")
            })
        });
    }
    group.finish();
}

/// Issue #711: the XPath scenario, Rift's slowest. Each of `count` stubs carries an XPath predicate
/// selecting a distinct node of one shared XML body; the request targets the last, so every stub's
/// XPath predicate is evaluated. Before #711 that meant `count` XML DOM parses **and** `count` XPath
/// compilations per request; after, one DOM parse and (warm) zero compilations. This is the
/// before/after headline for the slowest path.
///
/// XPath predicates are never candidate-indexable (they sit in `always_bits`), so the bitset
/// framework can't prune here — the win is purely the parse/compile-once change this benches.
fn bench_xpath_heavy(c: &mut Criterion) {
    let headers: HashMap<String, String> = HashMap::new();
    let mut group = c.benchmark_group("find_matching_stub_xpath_heavy");
    for count in [10usize, 100, 1000] {
        // Each stub extracts its own node and equals-checks it against "MATCH"; only the last node
        // holds "MATCH", so all `count` XPath predicates evaluate but only the last matches.
        let imposter = imposter_with_stubs(count, &|i| {
            json!({ "equals": { "body": "MATCH" },
                    "xpath": { "selector": format!("//item[@id='{i}']") } })
        });
        let body = format!(
            "<root>{}</root>",
            (0..count)
                .map(|i| format!(
                    "<item id='{i}'>{}</item>",
                    if i == count - 1 { "MATCH" } else { "x" }
                ))
                .collect::<String>()
        );
        assert!(
            imposter
                .find_matching_stub_with_client(
                    "POST",
                    "/x",
                    &headers,
                    None,
                    Some(&body),
                    None,
                    None
                )
                .expect("matching must not error")
                .is_some(),
            "xpath_heavy/{count}: fixture no longer matches its target stub",
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                imposter
                    .find_matching_stub_with_client(
                        "POST",
                        black_box("/x"),
                        black_box(&headers),
                        None,
                        black_box(Some(body.as_str())),
                        None,
                        None,
                    )
                    .expect("matching must not error")
            })
        });
    }
    group.finish();
}

/// Issue #708: the deepEquals-body dimension's headline. Every stub asserts one exact JSON body via
/// `deepEquals`, all sharing one path/method so no other dimension can prune — before #708 that meant
/// a full recursive structural comparison of every stub's body on every request (O(stubs × body)).
/// The request body equals the *last* stub's, so nothing short-circuits early. With the body-hash
/// dimension the candidate set collapses to ~1 via a single hash probe.
fn bench_deepequals_body(c: &mut Criterion) {
    let headers: HashMap<String, String> = HashMap::new();
    let mut group = c.benchmark_group("find_matching_stub_deepequals_body");
    for count in [10usize, 100, 500] {
        let imposter = imposter_with_stubs(
            count,
            &|i| json!({ "deepEquals": { "body": { "id": i, "kind": "order", "note": "x" } } }),
        );
        let body = format!(r#"{{"id":{},"kind":"order","note":"x"}}"#, count - 1);
        assert!(
            imposter
                .find_matching_stub_with_client(
                    "POST",
                    "/orders",
                    &headers,
                    None,
                    Some(&body),
                    None,
                    None
                )
                .expect("matching must not error")
                .is_some(),
            "deepequals_body/{count}: fixture no longer matches its target stub",
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                imposter
                    .find_matching_stub_with_client(
                        "POST",
                        black_box("/orders"),
                        black_box(&headers),
                        None,
                        black_box(Some(body.as_str())),
                        None,
                        None,
                    )
                    .expect("matching must not error")
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_stub_matches,
    bench_find_matching_stub,
    bench_method_dimension,
    bench_xpath_heavy,
    bench_deepequals_body
);
criterion_main!(benches);
