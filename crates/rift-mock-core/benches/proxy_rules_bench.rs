//! Criterion benches for the proxy's per-request rule scan (`extensions::matcher`).
//!
//! `handle_request` and `handle_script_rules` both walk their compiled rule list per request and
//! ask each rule `matches(method, uri, headers)` until one hits, so this scan is on every proxied
//! request — including the ones that match nothing and are forwarded untouched.
//!
//! The no-match case is the one worth watching: it is the common shape for a proxy with a handful
//! of narrowly-scoped rules, it costs a full scan (nothing short-circuits), and it is the path
//! issue #632 moved off eager `RequestInfo` construction. `rule_applies_to_upstream` is benched
//! too since it is `&&`-ed onto every one of those comparisons.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use hyper::{HeaderMap, Method, Uri};
use rift_mock_core::config::Rule;
use rift_mock_core::extensions::CompiledRule;
use rift_mock_core::proxy::rule_applies_to_upstream;
use serde_json::json;

fn compile(rule: serde_json::Value) -> CompiledRule {
    let rule: Rule = serde_json::from_value(rule).expect("valid rule fixture");
    CompiledRule::compile(rule).expect("rule compiles")
}

/// `count` rules, each narrowly scoped to its own exact path so a request for something else has
/// to be compared against every one of them.
fn exact_path_rules(count: usize) -> Vec<CompiledRule> {
    (0..count)
        .map(|i| {
            compile(json!({
                "id": format!("rule-{i}"),
                "match": { "methods": ["GET"], "path": { "exact": format!("/api/endpoint{i}") } },
                "fault": {},
            }))
        })
        .collect()
}

/// Rules that must run a regex against the path — the expensive per-rule comparison.
fn regex_path_rules(count: usize) -> Vec<CompiledRule> {
    (0..count)
        .map(|i| {
            compile(json!({
                "id": format!("rule-{i}"),
                "match": { "methods": ["GET"], "path": { "regex": format!("^/api/endpoint{i}/[0-9]+$") } },
                "fault": {},
            }))
        })
        .collect()
}

fn request_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("host", "api.example.com"),
        ("user-agent", "rift-bench/1.0"),
        ("accept", "application/json"),
        (
            "authorization",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ),
        ("x-tenant", "acme-corp"),
    ] {
        headers.insert(k, v.parse().expect("valid header value"));
    }
    headers
}

/// Scan the whole rule list without matching — the cost a proxied-but-unmatched request pays.
fn bench_rule_scan_no_match(c: &mut Criterion) {
    let headers = request_headers();
    let method = Method::GET;
    let uri: Uri = "/api/unmatched/path".parse().expect("valid uri");

    let mut group = c.benchmark_group("proxy_rule_scan_no_match");
    for count in [1usize, 10, 100] {
        let rules = exact_path_rules(count);
        // Guard the premise: if a fixture started matching, this would measure a short-circuited
        // scan while still reporting under the "no_match" name.
        assert!(
            !rules.iter().any(|r| r.matches(&method, &uri, &headers)),
            "exact/{count}: fixture must not match",
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("exact", count), &count, |b, _| {
            b.iter(|| {
                black_box(
                    rules.iter().any(|r| {
                        r.matches(black_box(&method), black_box(&uri), black_box(&headers))
                    }),
                )
            })
        });
    }
    for count in [1usize, 10, 100] {
        let rules = regex_path_rules(count);
        assert!(
            !rules.iter().any(|r| r.matches(&method, &uri, &headers)),
            "regex/{count}: fixture must not match",
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("regex", count), &count, |b, _| {
            b.iter(|| {
                black_box(
                    rules.iter().any(|r| {
                        r.matches(black_box(&method), black_box(&uri), black_box(&headers))
                    }),
                )
            })
        });
    }
    group.finish();
}

/// Matching on the last rule of the list: full scan plus a successful comparison.
fn bench_rule_scan_match_last(c: &mut Criterion) {
    let headers = request_headers();
    let method = Method::GET;

    let mut group = c.benchmark_group("proxy_rule_scan_match_last");
    for count in [1usize, 10, 100] {
        let rules = exact_path_rules(count);
        let uri: Uri = format!("/api/endpoint{}", count - 1)
            .parse()
            .expect("valid uri");
        // The point of this group is the full scan, so the hit must be the *last* rule.
        assert_eq!(
            rules
                .iter()
                .position(|r| r.matches(&method, &uri, &headers)),
            Some(count - 1),
            "{count}: fixture must match only the last rule",
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                black_box(rules.iter().position(|r| {
                    r.matches(black_box(&method), black_box(&uri), black_box(&headers))
                }))
            })
        });
    }
    group.finish();
}

/// A single rule's match cost, isolated by predicate kind — what each step of the scans above
/// actually pays for.
fn bench_single_rule_match(c: &mut Criterion) {
    let headers = request_headers();
    let method = Method::GET;
    let uri: Uri = "/api/v2/users/4815".parse().expect("valid uri");

    let mut group = c.benchmark_group("proxy_single_rule_match");

    let cases = [
        // `path` omitted -> PathMatch::Any (its serde default): matches every path.
        ("path_any", json!({ "methods": [] })),
        (
            "method_and_exact_path",
            json!({ "methods": ["GET"], "path": { "exact": "/api/v2/users/4815" } }),
        ),
        (
            "prefix_path",
            json!({ "methods": ["GET"], "path": { "prefix": "/api/v2/" } }),
        ),
        (
            "regex_path",
            json!({ "methods": ["GET"], "path": { "regex": r"^/api/v\d+/users/\d+$" } }),
        ),
        (
            "header_predicate",
            json!({
                "methods": ["GET"],
                "path": { "prefix": "/api/" },
                "headerPredicates": [{ "name": "x-tenant", "equals": "acme-corp" }],
            }),
        ),
    ];

    for (name, match_config) in cases {
        let rule = compile(json!({ "id": name, "match": match_config, "fault": {} }));
        // Each case is meant to price a *successful* match: a rule that fails short-circuits and
        // would quietly measure less work than its name implies.
        assert!(
            rule.matches(&method, &uri, &headers),
            "{name}: fixture must match",
        );
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(rule.matches(black_box(&method), black_box(&uri), black_box(&headers)))
            })
        });
    }

    group.finish();
}

/// `&&`-ed onto every rule comparison in the scan, so its cost rides along with each.
fn bench_rule_applies_to_upstream(c: &mut Criterion) {
    let mut group = c.benchmark_group("proxy_rule_applies_to_upstream");

    group.bench_function("no_filter", |b| {
        b.iter(|| {
            black_box(rule_applies_to_upstream(
                black_box(&None),
                black_box(Some("api")),
            ))
        })
    });

    let filter = Some("api".to_string());
    group.bench_function("matching_filter", |b| {
        b.iter(|| {
            black_box(rule_applies_to_upstream(
                black_box(&filter),
                black_box(Some("api")),
            ))
        })
    });

    group.bench_function("non_matching_filter", |b| {
        b.iter(|| {
            black_box(rule_applies_to_upstream(
                black_box(&filter),
                black_box(Some("other")),
            ))
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_rule_scan_no_match,
    bench_rule_scan_match_last,
    bench_single_rule_match,
    bench_rule_applies_to_upstream
);
criterion_main!(benches);
