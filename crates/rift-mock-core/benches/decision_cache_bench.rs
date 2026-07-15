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
//! * `decision_cache_payoff` — the memo cost side by side with the script execution it avoids
//!   (issue #665), so "does the cache pay for itself" is a measured ratio, not an argument.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rift_mock_core::extensions::NoOpFlowStore;
use rift_mock_core::imposter::ResponseMode;
use rift_mock_core::scripting::{
    CacheKey, CacheKeyBody, DecisionCache, DecisionCacheConfig, FaultDecision, RhaiEngine,
    ScriptRequest, execute_rhai_with_engine,
};
#[cfg(feature = "javascript")]
use rift_mock_core::scripting::{compile_js_to_bytecode, execute_js_bytecode};
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

/// The chunkier body fixture, shared by the key bench and the payoff bench (issue #665) so the
/// memo cost and the avoided cost are measured over the same payload.
fn large_body() -> serde_json::Value {
    json!({
        "items": (0..64)
            .map(|i| json!({ "sku": format!("SKU-{i:04}"), "qty": i % 7, "price": i as f64 * 1.5 }))
            .collect::<Vec<_>>(),
        "customer": { "id": "acme-corp", "tier": "enterprise" },
    })
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
                None,
                black_box(header_pairs.clone()),
                CacheKeyBody::Json(black_box(&small_body)),
                "bench-rule".to_string(),
            )
        })
    });

    let large_body = large_body();
    group.bench_function("large_body_64_items", |b| {
        b.iter(|| {
            CacheKey::new(
                "POST".to_string(),
                "/api/v2/orders".to_string(),
                None,
                black_box(header_pairs.clone()),
                CacheKeyBody::Json(black_box(&large_body)),
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
        None,
        header_pairs.clone(),
        CacheKeyBody::Json(&body),
        "bench-rule".to_string(),
    );
    cache.insert(key.clone(), decision());

    group.bench_function("hit", |b| b.iter(|| black_box(cache.get(black_box(&key)))));

    let absent = CacheKey::new(
        "POST".to_string(),
        "/api/v2/nonexistent".to_string(),
        None,
        header_pairs,
        CacheKeyBody::Json(&body),
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
                None,
                Vec::new(),
                CacheKeyBody::Json(&body),
                "bench-rule".to_string(),
            );
            cache.insert(black_box(key), decision());
        })
    });

    group.finish();
}

/// A script doing something ordinary — read a body field, decide a fault — per issue #665: a
/// no-op would flatter the memo, an elaborate one would flatter the cache.
const PAYOFF_JS_SCRIPT: &str = r#"
function respond(ctx) {
    var body = ctx.request.json;
    if (body && body.customer && body.customer.tier === "enterprise") {
        return delay(250);
    }
    return pass();
}
"#;

/// The same ordinary decision in Rhai — the cheaper engine, where the cache's margin is thinnest.
const PAYOFF_RHAI_SCRIPT: &str = r#"
fn respond(ctx) {
    let body = ctx.request.json;
    if body.customer.tier == "enterprise" {
        delay(250)
    } else {
        pass()
    }
}
"#;

/// The request a pool worker would receive for the payoff fixtures — same payload as the memo
/// side, with `raw_body` populated the way `proxy/handler.rs` populates it.
fn payoff_script_request(body: &serde_json::Value) -> ScriptRequest {
    ScriptRequest {
        method: "POST".to_string(),
        path: "/api/v2/orders".to_string(),
        headers: realistic_headers(),
        body: body.clone(),
        query: HashMap::new(),
        path_params: HashMap::new(),
        raw_body: Some(body.to_string()),
        mode: ResponseMode::Text,
    }
}

/// The cache's payoff, measured instead of argued (issue #665; the question behind #650/#654).
///
/// One group, two sides of the trade:
/// * `memo_hit_large_body` — what **every** request pays, hits included: `key_headers` +
///   `CacheKey::new` + `get`, exactly the sequence `proxy/handler.rs` runs before it can probe.
/// * `boa_execute_read_field` / `rhai_execute_read_field` — what a hit **avoids**: the worker-side
///   engine execution (`execute_js_bytecode` / `execute_rhai_with_engine` with a reusable engine,
///   the same calls a `ScriptPool` worker makes). Deliberately *without* the pool's queue hop,
///   oneshot and timeout plumbing, so the avoided cost is understated — if the memo wins against
///   the bare execution, it wins against the full round-trip a fortiori.
///
/// The ratio `avoided / memo` is the answer to "does the cache pay for itself"; read it off the
/// group's report before re-filing #650 a third time. Snapshot at filing time (Apple Silicon,
/// 2026-07): memo ~2.4 µs vs Boa ~171 µs (~70x) and Rhai ~27 µs (~11x) — the margin holds with
/// two orders of magnitude to spare on the engine the cache was built for, and one on Rhai.
fn bench_payoff(c: &mut Criterion) {
    let mut group = c.benchmark_group("decision_cache_payoff");

    let body = large_body();
    let headers = realistic_headers();

    // The memo side: worst-case config (`None` = key on every header) over the same payload.
    let cache = cache_with(None);
    let key = CacheKey::new(
        "POST".to_string(),
        "/api/v2/orders".to_string(),
        None,
        cache.key_headers(&headers),
        CacheKeyBody::Json(&body),
        "bench-rule".to_string(),
    );
    cache.insert(key, decision());

    group.bench_function("memo_hit_large_body", |b| {
        b.iter(|| {
            let key = CacheKey::new(
                "POST".to_string(),
                "/api/v2/orders".to_string(),
                None,
                cache.key_headers(black_box(&headers)),
                CacheKeyBody::Json(black_box(&body)),
                "bench-rule".to_string(),
            );
            black_box(cache.get(&key))
        })
    });

    // The avoided side: one engine execution of the ordinary script over the same payload.
    let request = payoff_script_request(&body);

    #[cfg(feature = "javascript")]
    {
        let bytecode = compile_js_to_bytecode(PAYOFF_JS_SCRIPT).expect("payoff JS compiles");
        group.bench_function("boa_execute_read_field", |b| {
            b.iter(|| {
                execute_js_bytecode(
                    black_box(&bytecode),
                    black_box(&request),
                    Arc::new(NoOpFlowStore),
                    "bench-rule",
                )
                .expect("payoff JS executes")
            })
        });
    }

    let rhai_engine = RhaiEngine::create_engine();
    let rhai_ast = RhaiEngine::new(PAYOFF_RHAI_SCRIPT, "bench-rule")
        .expect("payoff Rhai compiles")
        .ast()
        .clone();
    group.bench_function("rhai_execute_read_field", |b| {
        b.iter(|| {
            execute_rhai_with_engine(
                &rhai_engine,
                black_box(&rhai_ast),
                black_box(&request),
                Arc::new(NoOpFlowStore),
                "bench-rule",
            )
            .expect("payoff Rhai executes")
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_key_headers,
    bench_cache_key_new,
    bench_get,
    bench_insert,
    bench_payoff
);
criterion_main!(benches);
