//! Criterion benches for the scripting decision cache (`scripting::decision_cache`).
//!
//! This is the proxy's per-request script-decision memo: every request that hits a script rule
//! builds a `CacheKey` and probes the cache, so key construction is as hot as the lookup itself
//! and is on the critical path even when the cache hits.
//!
//! Covers the shapes the recent perf work changed:
//! * `key_headers` — the `None` (key on every header) vs `Some(allow)` (issue #630/#643) split.
//! * `CacheKey::new` — header sort + body hash, paid once per request.
//! * `get`/`insert` — the `LruCache` critical section (issue #631).

use std::cell::Cell;
use std::collections::HashMap;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rift_mock_core::scripting::{CacheKey, DecisionCache, DecisionCacheConfig, FaultDecision};
use serde_json::json;

/// A header set shaped like a real proxied request: a handful the scripts care about, plus the
/// ambient noise (tracing, agent, encoding) that a `None` key_headers config also hashes.
fn realistic_headers() -> HashMap<String, String> {
    [
        ("host", "api.example.com"),
        ("user-agent", "rift-bench/1.0"),
        ("accept", "application/json"),
        ("accept-encoding", "gzip, deflate, br"),
        ("content-type", "application/json"),
        ("content-length", "128"),
        (
            "authorization",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ),
        ("x-tenant", "acme-corp"),
        ("x-request-id", "0f8fad5b-d9cb-469f-a165-70867728950e"),
        (
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ),
        ("x-forwarded-for", "203.0.113.7"),
        ("cookie", "session=abc123; consent=1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn cache_with(key_headers: Option<Vec<String>>) -> DecisionCache {
    DecisionCache::new(DecisionCacheConfig {
        enabled: true,
        max_size: 10_000,
        ttl_seconds: 300,
        key_headers,
    })
}

fn decision() -> FaultDecision {
    FaultDecision::Latency {
        duration_ms: 250,
        rule_id: "bench-rule".to_string(),
    }
}

/// The header-subset step (issue #630). `None` clones every header into the key; `Some(allow)`
/// keeps only the declared ones. The gap here is the per-request cost of *not* declaring
/// `key_headers` — and, since an undeclared per-request-unique header (`x-request-id`,
/// `traceparent` above) also makes every key unique, the cost of a cache that never hits.
fn bench_key_headers(c: &mut Criterion) {
    let mut group = c.benchmark_group("decision_cache_key_headers");
    let headers = realistic_headers();

    let all = cache_with(None);
    group.bench_function("all_headers", |b| {
        b.iter(|| black_box(all.key_headers(black_box(&headers))))
    });

    // The headers a script realistically reads, declared explicitly.
    let allowlist = cache_with(Some(vec![
        "x-tenant".to_string(),
        "authorization".to_string(),
    ]));
    group.bench_function("allowlist_2_of_12", |b| {
        b.iter(|| black_box(allowlist.key_headers(black_box(&headers))))
    });

    group.finish();
}

/// Key construction: sorts the header pairs and hashes the body to a `u64`. Scales with body size,
/// so bench a small JSON body against a chunkier one.
fn bench_cache_key_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("decision_cache_key_new");

    let cache = cache_with(None);
    let header_pairs = cache.key_headers(&realistic_headers());

    let small_body = json!({ "action": "purchase", "id": 4815 });
    group.bench_function("small_body", |b| {
        b.iter(|| {
            CacheKey::new(
                "POST".to_string(),
                "/api/v2/orders".to_string(),
                black_box(header_pairs.clone()),
                black_box(&small_body),
                "bench-rule".to_string(),
            )
        })
    });

    let large_body = json!({
        "items": (0..64)
            .map(|i| json!({ "sku": format!("SKU-{i:04}"), "qty": i % 7, "price": i as f64 * 1.5 }))
            .collect::<Vec<_>>(),
        "customer": { "id": "acme-corp", "tier": "enterprise" },
    });
    group.bench_function("large_body_64_items", |b| {
        b.iter(|| {
            CacheKey::new(
                "POST".to_string(),
                "/api/v2/orders".to_string(),
                black_box(header_pairs.clone()),
                black_box(&large_body),
                "bench-rule".to_string(),
            )
        })
    });

    group.finish();
}

/// The locked `LruCache` section (issue #631). A hit clones the decision out and bumps recency; a
/// miss touches the miss counter. Both hold the same mutex, so this is the contention-free floor.
fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("decision_cache_get");

    let cache = cache_with(None);
    let header_pairs = cache.key_headers(&realistic_headers());
    let body = json!({ "action": "purchase", "id": 4815 });
    let key = CacheKey::new(
        "POST".to_string(),
        "/api/v2/orders".to_string(),
        header_pairs.clone(),
        &body,
        "bench-rule".to_string(),
    );
    cache.insert(key.clone(), decision());

    group.bench_function("hit", |b| b.iter(|| black_box(cache.get(black_box(&key)))));

    let absent = CacheKey::new(
        "POST".to_string(),
        "/api/v2/nonexistent".to_string(),
        header_pairs,
        &body,
        "bench-rule".to_string(),
    );
    group.bench_function("miss", |b| {
        b.iter(|| black_box(cache.get(black_box(&absent))))
    });

    group.finish();
}

/// Steady-state insert at capacity: every insert of a fresh key evicts the LRU victim, which is
/// the shape a degenerate (per-request-unique) key produces in production.
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("decision_cache_insert");

    let cache = DecisionCache::new(DecisionCacheConfig {
        enabled: true,
        max_size: 1_000,
        ttl_seconds: 300,
        key_headers: Some(Vec::new()),
    });
    let body = json!({ "action": "purchase" });
    let counter = Cell::new(0u64);

    group.bench_function("evicting_at_capacity", |b| {
        b.iter(|| {
            let i = counter.get();
            counter.set(i + 1);
            let key = CacheKey::new(
                "POST".to_string(),
                format!("/api/v2/orders/{i}"),
                Vec::new(),
                &body,
                "bench-rule".to_string(),
            );
            cache.insert(black_box(key), decision());
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_key_headers,
    bench_cache_key_new,
    bench_get,
    bench_insert
);
criterion_main!(benches);
