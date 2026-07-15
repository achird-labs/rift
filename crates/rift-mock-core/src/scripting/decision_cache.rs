use crate::scripting::FaultDecision;
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

/// Configuration for the decision cache
#[derive(Clone, Debug)]

pub struct DecisionCacheConfig {
    /// Enable decision caching
    pub enabled: bool,
    /// Maximum number of cache entries (LRU eviction when exceeded)
    pub max_size: usize,
    /// TTL for cache entries in seconds (0 = no expiration)
    pub ttl_seconds: u64,
    /// Headers that participate in the cache key. `None` keys on every header (issue #630).
    pub key_headers: Option<Vec<String>>,
}

impl Default for DecisionCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: 10000,
            ttl_seconds: 300, // 5 minutes
            key_headers: None,
        }
    }
}

/// How the request body participates in the cache key (issue #652).
///
/// The memoised function is a user script, and it reads the body as `ctx.request.raw_body` — so the
/// key must cover the *bytes*, not only whatever they parsed into. The caller hands [`Json`] when
/// the body parsed and [`Raw`] otherwise: binary, text, malformed JSON, and the empty body.
///
/// [`Json`]: CacheKeyBody::Json
/// [`Raw`]: CacheKeyBody::Raw
#[derive(Clone, Copy, Debug)]
pub enum CacheKeyBody<'a> {
    /// The body parsed as JSON. Hashed structurally, so formatting and key order — which a script
    /// cannot observe through `ctx.request.body` — do not split the key (issue #653).
    Json(&'a serde_json::Value),
    /// The body did not parse as JSON, so the script sees `body == null` and branches on the bytes
    /// instead. Hashed as bytes, which is also cheaper than the JSON tree walk.
    Raw(&'a [u8]),
}

/// Domain tags mixed into the body hash so [`CacheKeyBody::Json`] and [`CacheKeyBody::Raw`] are
/// hashed in separate domains — a raw body can never share a hash with the JSON it happens to
/// spell, by construction rather than by luck.
const JSON_BODY_TAG: u8 = 0;
const RAW_BODY_TAG: u8 = 1;

/// The same technique for the query (issue #660): `/path` (no query at all) and `/path?` (an empty
/// one) are different requests, so their hashes must differ by construction rather than by luck.
const QUERY_TAG: u8 = 0;
const NO_QUERY_TAG: u8 = 1;

/// Cache key derived from request properties
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    /// Request method
    method: String,
    /// Request path
    path: String,
    /// Sorted header keys and values (for deterministic hashing)
    headers: Vec<(String, String)>,
    /// Query hash (issue #660). Hashed rather than stored: a query string is attacker-long
    /// (~8 KB URLs) against a 10k-entry LRU, where `path` is bounded in practice.
    query_hash: u64,
    /// Body hash (to avoid storing large bodies)
    body_hash: u64,
    /// Rule ID
    rule_id: String,
}

impl CacheKey {
    /// Create a new cache key from request properties
    pub fn new(
        method: String,
        path: String,
        query: Option<&str>,
        mut headers: Vec<(String, String)>,
        body: CacheKeyBody<'_>,
        rule_id: String,
    ) -> Self {
        // Sort headers for deterministic key generation
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        // Hash the body to avoid storing large payloads
        let body_hash = Self::hash_body(body);
        let query_hash = Self::hash_query(query);

        Self {
            method,
            path,
            headers,
            query_hash,
            body_hash,
            rule_id,
        }
    }

    /// Hash the raw query string for the cache key, tagged so an absent query and an empty one
    /// cannot collide (issue #660).
    ///
    /// The **raw** string, deliberately, not the parsed map the script actually reads
    /// (`ctx.request.query`). The map would be faithful today, but `parse_query_string` is lossy —
    /// last-wins on duplicate keys, percent-decoding, order erased — so keying on it would need a
    /// canonical ordering pass per request, and would silently under-split again the day a script
    /// gains a raw-query or full-URI accessor. The raw string can only err the safe way: the same
    /// raw string always parses to the same map, so it never under-splits; two spellings of one
    /// logical query (`?a=1&b=2` vs `?b=2&a=1`) over-split, costing hit rate and nothing else.
    ///
    /// `FixedState` for the same reason as [`hash_body`](Self::hash_body): this `u64` is stored in
    /// the key and compared by `Eq`, so a per-call seed would stop the cache ever hitting.
    fn hash_query(query: Option<&str>) -> u64 {
        let mut hasher = foldhash::fast::FixedState::default().build_hasher();
        match query {
            Some(q) => {
                QUERY_TAG.hash(&mut hasher);
                q.hash(&mut hasher);
            }
            None => NO_QUERY_TAG.hash(&mut hasher),
        }
        hasher.finish()
    }

    /// Hash the body for the cache key, tagged by domain (issue #652).
    ///
    /// The tag is what makes [`CacheKeyBody::Json`] and [`CacheKeyBody::Raw`] incapable of sharing
    /// a hash: without it, `Raw(br#"{"a":1}"#)` and the `Value` it parses to would be two spellings
    /// of one key. It also costs nothing — one byte into the hasher that is already running.
    ///
    /// The `Raw` arm exists because the memoised script reads `ctx.request.raw_body`. Keying only
    /// on the parsed `Value` meant every non-JSON body — every binary upload, every text payload,
    /// the empty body — hashed as `Null` and shared **one** entry, so the second request was served
    /// the first's fault decision with nothing logged (issue #652). It is also the cheaper arm: a
    /// contiguous byte hash rather than a tree walk.
    ///
    /// ## The `Json` arm
    ///
    /// Walks the `Value` in place. This previously serialised a canonical JSON `String` first,
    /// which put an allocation and a full render of the body on the per-request hot path — the key
    /// is built before the cache can be probed, so every request paid it, hits included, and for a
    /// chunky body it cost multiples of the lookup it exists to memoise (issue #650 has the
    /// measurements).
    ///
    /// `serde_json`'s own impls provide the canonicality the `String` was buying: `Hash for
    /// Map<String, Value>` sorts its keys when `preserve_order` is on and is a sorted `BTreeMap`
    /// otherwise, and `Value`'s derived `Hash` mixes in a variant discriminant so distinct shapes
    /// stay distinct. That is strictly better than the render it replaces — under `preserve_order`,
    /// `to_string` would have emitted insertion order and silently stopped being canonical.
    ///
    /// It also fixes one key split: `-0.0` and `0.0` are `==` as `Value`s, but rendered to
    /// different strings, so the old hash violated `k1 == k2 ⟹ hash(k1) == hash(k2)` and gave them
    /// two cache entries. `Hash for Number` folds them onto one, which is what equality already said.
    ///
    /// Caveat inherited from `Hash for serde_json::Number`: it hashes the numeric payload without
    /// an arm tag, so values that differ only across `u64`/`i64`/`f64` but share a bit pattern
    /// (`18446744073709551615` vs `-1`; `1.0` vs `4607182418800017408`) collide where the old
    /// render did not. This is accepted rather than fixed by hand-rolling a tagged walk over
    /// `Number`. It takes two requests whose bodies differ *only* in a number that spans those arms
    /// with a shared bit pattern, at the same method/path/keyed-headers/rule; the cost is one wrong
    /// cached decision for one TTL, and re-owning serde_json's number hashing to prevent it costs
    /// more than that is worth.
    fn hash_body(body: CacheKeyBody<'_>) -> u64 {
        // `foldhash` rather than SipHash (`DefaultHasher`): this hash never leaves the process —
        // it is an in-memory LRU key, never persisted and never sent over a wire — so it has no
        // stability requirement across builds or runs, and SipHash's collision resistance buys
        // nothing here. It costs, though: walking a ~200-node `Value` tree through SipHash was the
        // dominant term in key construction (issue #654).
        //
        // `FixedState`, NOT `RandomState`: this `u64` is *stored* in the key and compared by `Eq`,
        // so a per-call random seed would give the same body a different hash on every request and
        // the cache would never hit again — silently, with every correctness test still green.
        // (The LRU map's own hasher is randomly seeded; see `DecisionCache::new`.)
        let mut hasher = foldhash::fast::FixedState::default().build_hasher();
        match body {
            CacheKeyBody::Json(value) => {
                JSON_BODY_TAG.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            CacheKeyBody::Raw(bytes) => {
                RAW_BODY_TAG.hash(&mut hasher);
                // `Hash for [u8]` writes the length before the bytes, so no two byte strings can
                // be confused by concatenation.
                bytes.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

/// Cache entry with TTL tracking
#[derive(Debug)]
struct CacheEntry {
    decision: FaultDecision,
    created_at: Instant,
}

impl CacheEntry {
    fn new(decision: FaultDecision) -> Self {
        Self {
            decision,
            created_at: Instant::now(),
        }
    }

    fn is_expired(&self, ttl: Duration) -> bool {
        if ttl.is_zero() {
            return false; // No expiration
        }
        self.created_at.elapsed() > ttl
    }
}

/// Metrics for cache performance
#[derive(Clone, Debug, Default)]

pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub expirations: u64,
    pub size: usize,
}

impl CacheMetrics {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// A cache doing no useful work is worse than no cache — it pays hashing, allocation and lock
/// traffic per request and returns nothing — but nothing consumes `metrics()`, so the state is
/// otherwise invisible (issue #630). These bound a once-per-process warning.
const DEGENERATE_MIN_LOOKUPS: u64 = 4096;
const DEGENERATE_HIT_RATE: f64 = 0.01;

/// True when enough lookups have happened to judge, and effectively none of them hit. Pure so the
/// threshold logic is testable without a log-capture subscriber.
fn is_degenerate(hits: u64, misses: u64) -> bool {
    let total = hits + misses;
    total >= DEGENERATE_MIN_LOOKUPS && (hits as f64 / total as f64) < DEGENERATE_HIT_RATE
}

/// Counters kept outside the cache lock. Each is updated independently, so a `metrics()` snapshot
/// is not a consistent point-in-time view across counters — nothing consumes them that way today.
#[derive(Debug, Default)]
struct AtomicMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    evictions: AtomicU64,
    expirations: AtomicU64,
}

/// Decision cache for memoizing script execution results
pub struct DecisionCache {
    config: DecisionCacheConfig,
    /// `None` when the cache is off — either `enabled == false` or a zero `max_size` — so a
    /// disabled cache has no storage to accidentally read or grow (issue #544).
    ///
    /// A `Mutex` rather than an `RwLock`: an LRU lookup reorders the recency list, so every `get`
    /// mutates and a read lock was never honest. The win here is a bounded critical section —
    /// `LruCache::push` evicts in O(1), where the previous `min_by_key` scanned all 10k entries
    /// under the exclusive lock on every steady-state insert.
    cache: Option<Mutex<LruCache<CacheKey, CacheEntry, foldhash::fast::RandomState>>>,
    metrics: AtomicMetrics,
    /// Latches the degenerate-cache warning so a pathological key shape is reported once, not
    /// once per request.
    degenerate_warned: AtomicBool,
}

impl DecisionCache {
    /// Create a new decision cache
    pub fn new(config: DecisionCacheConfig) -> Self {
        debug!(
            "Creating decision cache: enabled={}, max_size={}, ttl={}s",
            config.enabled, config.max_size, config.ttl_seconds
        );

        // `foldhash` for the map too (issue #654): `get`/`push` re-hash the *whole* key — the
        // method/path/rule strings and every kept header — through the map's build hasher on every
        // probe, so the lookup side pays SipHash as well, not just the body hash.
        //
        // `RandomState` here, unlike `hash_body`: the map holds one instance, so it is internally
        // consistent for its whole life, and a per-process seed keeps keys built from
        // network-controlled input (path, headers, body) from being collided on purpose.
        let cache = match (config.enabled, NonZeroUsize::new(config.max_size)) {
            (true, Some(capacity)) => Some(Mutex::new(LruCache::with_hasher(
                capacity,
                foldhash::fast::RandomState::default(),
            ))),
            _ => None,
        };

        Self {
            config,
            cache,
            metrics: AtomicMetrics::default(),
            degenerate_warned: AtomicBool::new(false),
        }
    }

    /// Get a decision from cache if available and not expired
    pub fn get(&self, key: &CacheKey) -> Option<FaultDecision> {
        let cache = self.cache.as_ref()?;
        let ttl = Duration::from_secs(self.config.ttl_seconds);

        let mut cache = cache.lock();
        // `Some(None)` = present but expired; the decision is only cloned on a live hit.
        let hit = cache.get(key).map(|entry| {
            if entry.is_expired(ttl) {
                None
            } else {
                Some(entry.decision.clone())
            }
        });

        match hit {
            Some(None) => {
                trace!("Cache entry expired for key: {:?}", key);
                cache.pop(key);
                self.metrics.misses.fetch_add(1, Ordering::Relaxed);
                self.metrics.expirations.fetch_add(1, Ordering::Relaxed);
                None
            }
            Some(Some(decision)) => {
                trace!("Cache hit for key: {:?}", key);
                self.metrics.hits.fetch_add(1, Ordering::Relaxed);
                Some(decision)
            }
            None => {
                trace!("Cache miss for key: {:?}", key);
                let misses = self.metrics.misses.fetch_add(1, Ordering::Relaxed) + 1;
                // Sampled: the check reads a second atomic and does a float divide, so it must not
                // ride every miss. Only the true-miss arm samples — an expiring entry means the key
                // IS being reused, which is the opposite of the shape being detected.
                if misses.is_multiple_of(DEGENERATE_MIN_LOOKUPS) {
                    self.warn_if_degenerate(misses);
                }
                None
            }
        }
    }

    /// Report a cache that is doing no useful work — once per process (issue #630).
    ///
    /// The usual cause is a per-request-unique header (`x-request-id`, `traceparent`, `date`) in
    /// the key, which makes every key unique: 0% hits, and the cache is pure overhead on the hot
    /// path. Without this the state is silent, because nothing reads `metrics()`.
    fn warn_if_degenerate(&self, misses: u64) {
        let hits = self.metrics.hits.load(Ordering::Relaxed);
        if !is_degenerate(hits, misses) {
            return;
        }
        if self.degenerate_warned.swap(true, Ordering::Relaxed) {
            return;
        }
        warn!(
            hits,
            misses,
            "decision cache hit rate is ~0%: it is costing more than it saves. A per-request-unique \
             header (x-request-id, traceparent, date) in the cache key makes every key unique — set \
             `scripting.decision_cache.key_headers` to the headers your scripts actually read."
        );
    }

    /// The header subset that participates in the cache key (issue #630).
    ///
    /// `None` keys on every header — correct but degenerate whenever any header is per-request
    /// unique. `Some(allow)` is the user asserting their scripts' decisions depend on at most
    /// those headers; the key cannot be narrowed automatically, because the cached value is a
    /// user script's decision and the script is handed every header. Matching is
    /// case-insensitive: config is human-written (`X-Tenant`), the wire name arrives lowercased.
    pub fn key_headers(&self, headers: &HashMap<String, String>) -> Vec<(String, String)> {
        match &self.config.key_headers {
            None => headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            Some(allow) => headers
                .iter()
                .filter(|(k, _)| allow.iter().any(|a| a.eq_ignore_ascii_case(k)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Insert a decision into the cache
    pub fn insert(&self, key: CacheKey, decision: FaultDecision) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };

        let mut cache = cache.lock();
        // `push` returns the displaced pair: the LRU victim when at capacity, or the previous
        // value when this key was already present. Only the former is an eviction.
        let replacing = cache.contains(&key);
        let displaced = cache.push(key, CacheEntry::new(decision));
        if displaced.is_some() && !replacing {
            self.metrics.evictions.fetch_add(1, Ordering::Relaxed);
        }

        self.metrics.inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear all cache entries
    pub fn clear(&self) {
        if let Some(cache) = self.cache.as_ref() {
            cache.lock().clear();
        }
        debug!("Cache cleared");
    }

    /// Get current cache metrics
    pub fn metrics(&self) -> CacheMetrics {
        CacheMetrics {
            hits: self.metrics.hits.load(Ordering::Relaxed),
            misses: self.metrics.misses.load(Ordering::Relaxed),
            inserts: self.metrics.inserts.load(Ordering::Relaxed),
            evictions: self.metrics.evictions.load(Ordering::Relaxed),
            expirations: self.metrics.expirations.load(Ordering::Relaxed),
            size: self.size(),
        }
    }

    /// Remove expired entries (can be called periodically)
    pub fn cleanup_expired(&self) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        if self.config.ttl_seconds == 0 {
            return;
        }

        let ttl = Duration::from_secs(self.config.ttl_seconds);
        let mut cache = cache.lock();

        let expired_keys: Vec<CacheKey> = cache
            .iter()
            .filter(|(_, entry)| entry.is_expired(ttl))
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            cache.pop(&key);
        }

        if count > 0 {
            debug!("Cleaned up {} expired cache entries", count);
            self.metrics
                .expirations
                .fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    /// Get cache size
    pub fn size(&self) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.lock().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread;

    /// Counts allocations made by the calling thread, so `hashing_a_body_allocates_nothing` can
    /// assert on the property issue #650 is actually about. Thread-local rather than global
    /// because the rest of this binary's tests run in parallel and would otherwise pollute the
    /// count. `const`-initialised and `Cell<usize>` (no destructor) so the TLS access itself
    /// cannot allocate and re-enter the allocator.
    ///
    /// This does NOT breach the rule stated in `rift-http-proxy`'s `main.rs` — "the allocator is
    /// set only here in the binary, never in the rift-mock-core/rift-ffi libs" (issue #293).
    /// `rift-mock-core` is meant to be embedded, and a lib that hijacks its host's allocator is
    /// exactly what #293 forbids; this one is `cfg(test)`, so it exists only in this crate's own
    /// test harness and never in anything an embedder links. Note a binary may have only one:
    /// a second allocator-swapping test anywhere in this crate collides with this at compile time.
    mod counting_alloc {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static ALLOCS: Cell<usize> = const { Cell::new(0) };
        }

        pub fn count() -> usize {
            ALLOCS.with(Cell::get)
        }

        fn record() {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }

        pub struct Counting;

        // SAFETY: every method forwards to `System`, a valid allocator, with the pointer/layout it
        // was given; the counter is a side effect that touches no allocator state. `realloc` and
        // `alloc_zeroed` are forwarded explicitly rather than left to the trait defaults: the
        // defaults reroute through `alloc`+copy+`dealloc`, which would cost every `Vec` growth in
        // the whole test binary its grow-in-place path.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                record();
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                record();
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                record();
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }
    }

    #[global_allocator]
    static COUNTING_ALLOC: counting_alloc::Counting = counting_alloc::Counting;

    /// A body shaped like the bench's `large_body_64_items` — a plain JSON order payload, not a
    /// pathological fixture.
    fn large_body() -> serde_json::Value {
        json!({
            "items": (0..64)
                .map(|i| json!({ "sku": format!("SKU-{i:04}"), "qty": i % 7, "price": i as f64 * 1.5 }))
                .collect::<Vec<_>>(),
            "customer": { "id": "acme-corp", "tier": "enterprise" },
        })
    }

    /// Issue #650. The body hash is unconditional on the script hot path — `proxy/handler.rs`
    /// builds the key *before* it can probe the cache — so every request pays it, hits included.
    /// Serialising a canonical `String` first cost 5.70 µs on this body to memoise a 122 ns
    /// lookup, i.e. the cache lost to the thing it memoises. Hashing must walk the `Value` in
    /// place, which allocates nothing.
    #[test]
    fn hashing_a_body_allocates_nothing() {
        let body = large_body();
        // Warm anything lazy (hasher construction, TLS) so the measured window is the hash alone.
        let _ = CacheKey::hash_body(CacheKeyBody::Json(&body));

        let before = counting_alloc::count();
        std::hint::black_box(CacheKey::hash_body(CacheKeyBody::Json(
            std::hint::black_box(&body),
        )));
        let allocations = counting_alloc::count() - before;

        assert_eq!(
            allocations, 0,
            "hashing a body must not allocate; {allocations} allocation(s) means the hash is \
             still materialising an intermediate (issue #650)"
        );
    }

    /// The property the discarded `to_string` was buying: two bodies equal as JSON must share a
    /// cache entry however their keys were ordered on the wire. Losing it would split the key on
    /// nothing and quietly cost hits.
    #[test]
    fn body_hash_is_insertion_order_independent() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"x":{"p":1,"q":[1,2]},"y":null,"z":"s"}"#).expect("valid");
        let b: serde_json::Value =
            serde_json::from_str(r#"{"z":"s","y":null,"x":{"q":[1,2],"p":1}}"#).expect("valid");

        assert_eq!(a, b, "fixture precondition: these are the same JSON value");
        assert_eq!(
            CacheKey::hash_body(CacheKeyBody::Json(&a)),
            CacheKey::hash_body(CacheKeyBody::Json(&b)),
            "equal JSON must hash equally whatever order the keys arrived in"
        );
    }

    /// `-0.0` and `0.0` are `==` as `Value`s, so hashing them apart broke
    /// `k1 == k2 ⟹ hash(k1) == hash(k2)` and handed them two cache entries for one value. The old
    /// `to_string` did exactly that (`"-0.0"` vs `"0.0"`); `Hash for Number` folds them. This is a
    /// behaviour change — one fewer key — and it is the correct direction.
    #[test]
    fn body_hash_folds_negative_zero_onto_zero() {
        assert_eq!(
            json!(-0.0),
            json!(0.0),
            "fixture precondition: these are == as Values"
        );
        assert_eq!(
            CacheKey::hash_body(CacheKeyBody::Json(&json!(-0.0))),
            CacheKey::hash_body(CacheKeyBody::Json(&json!(0.0))),
            "values that compare equal must hash equally, or they split the key on nothing"
        );
    }

    /// Pins a KNOWN, ACCEPTED collision rather than asserting it is desirable — so that a
    /// serde_json bump, a hasher swap, or someone deliberately closing it cannot move the
    /// collision surface unnoticed. `Hash for serde_json::Number` hashes the payload with no arm
    /// tag, and `i64::hash` writes `i as u64`, so every integer in `[i64::MAX + 1, u64::MAX]`
    /// collides with its two's-complement negative. The old `to_string` hash did not do this.
    /// Accepted because the caller already collapses every non-JSON body onto `Value::Null`
    /// (issue #652), an incomparably larger class. If this test fails, that trade-off changed and
    /// the doc comment on `hash_body` needs revisiting — it is not automatically a bug.
    #[test]
    fn known_accepted_collision_across_number_arms_is_unchanged() {
        assert_ne!(
            json!(u64::MAX),
            json!(-1i64),
            "fixture precondition: these are different JSON values"
        );
        assert_eq!(
            CacheKey::hash_body(CacheKeyBody::Json(&json!(u64::MAX))),
            CacheKey::hash_body(CacheKeyBody::Json(&json!(-1i64))),
            "documented accepted collision: u64/i64 arms share a bit pattern (see hash_body's docs)"
        );
    }

    /// A `u64` body hash *is* the key — `CacheKey`'s `Eq` compares the hash, never the body — so a
    /// shape collision serves one body's script decision to a different body. Distinct shapes must
    /// stay distinct.
    #[test]
    fn distinct_json_shapes_produce_distinct_body_hashes() {
        let shapes = [
            json!(null),
            json!("null"),
            json!(true),
            json!("true"),
            json!(1),
            json!(1.0),
            json!("1"),
            json!([]),
            json!({}),
            json!([1]),
            json!([1, 1]),
            json!({ "a": 1 }),
            json!({ "a": "1" }),
            json!({ "a": 1, "b": 2 }),
            json!([["a", 1]]),
        ];

        for (i, a) in shapes.iter().enumerate() {
            for b in &shapes[i + 1..] {
                assert_ne!(
                    CacheKey::hash_body(CacheKeyBody::Json(a)),
                    CacheKey::hash_body(CacheKeyBody::Json(b)),
                    "distinct JSON shapes must not share a cache key: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn test_cache_key_creation() {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-request-id".to_string(), "123".to_string()),
        ];

        let key1 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            headers.clone(),
            CacheKeyBody::Json(&json!({"foo": "bar"})),
            "rule1".to_string(),
        );

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            headers.clone(),
            CacheKeyBody::Json(&json!({"foo": "bar"})),
            "rule1".to_string(),
        );

        // Same inputs should produce equal keys
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_cache_key_different_order_headers() {
        let headers1 = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ];

        let headers2 = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ];

        let key1 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            headers1,
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            headers2,
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        // Headers in different order should produce same key
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_cache_basic_operations() {
        let config = DecisionCacheConfig {
            enabled: true,
            max_size: 100,
            ttl_seconds: 0, // No expiration for this test
            key_headers: None,
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        // Cache miss
        assert!(cache.get(&key).is_none());

        // Insert
        let decision = FaultDecision::Latency {
            duration_ms: 100,
            rule_id: "rule1".to_string(),
        };
        cache.insert(key.clone(), decision.clone());

        // Cache hit
        let cached = cache.get(&key).unwrap();
        match cached {
            FaultDecision::Latency { duration_ms, .. } => {
                assert_eq!(duration_ms, 100);
            }
            _ => panic!("Expected Latency decision"),
        }

        // Verify metrics
        let metrics = cache.metrics();
        assert_eq!(metrics.hits, 1);
        assert_eq!(metrics.misses, 1);
        assert_eq!(metrics.inserts, 1);
        assert_eq!(metrics.size, 1);
    }

    #[test]
    fn test_cache_expiration() {
        let config = DecisionCacheConfig {
            enabled: true,
            max_size: 100,
            ttl_seconds: 1, // 1 second TTL
            key_headers: None,
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        let decision = FaultDecision::None;
        cache.insert(key.clone(), decision);

        // Should be cached
        assert!(cache.get(&key).is_some());

        // Wait for expiration
        thread::sleep(Duration::from_secs(2));

        // Should be expired
        assert!(cache.get(&key).is_none());

        // Verify expiration metric
        let metrics = cache.metrics();
        assert_eq!(metrics.expirations, 1);
    }

    #[test]
    fn test_cache_lru_eviction() {
        let config = DecisionCacheConfig {
            enabled: true,
            max_size: 3,
            ttl_seconds: 0,
            key_headers: None,
        };

        let cache = DecisionCache::new(config);

        // Insert 3 entries
        for i in 0..3 {
            let key = CacheKey::new(
                "GET".to_string(),
                format!("/api/test{i}"),
                None,
                vec![],
                CacheKeyBody::Json(&json!({})),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None);
        }

        assert_eq!(cache.size(), 3);

        // Access key 1 and 2 to make key 0 the LRU
        let key1 = CacheKey::new(
            "GET".to_string(),
            "/api/test1".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );
        cache.get(&key1);

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test2".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule2".to_string(),
        );
        cache.get(&key2);

        // Insert 4th entry - should evict key 0 (LRU)
        let key3 = CacheKey::new(
            "GET".to_string(),
            "/api/test3".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule3".to_string(),
        );
        cache.insert(key3, FaultDecision::None);

        assert_eq!(cache.size(), 3);

        // Key 0 should be evicted
        let key0 = CacheKey::new(
            "GET".to_string(),
            "/api/test0".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule0".to_string(),
        );
        assert!(cache.get(&key0).is_none());

        // Keys 1, 2, 3 should still be present
        assert!(cache.get(&key1).is_some());
        assert!(cache.get(&key2).is_some());

        let metrics = cache.metrics();
        assert_eq!(metrics.evictions, 1);
    }

    #[test]
    fn test_cache_disabled() {
        let config = DecisionCacheConfig {
            enabled: false,
            max_size: 100,
            ttl_seconds: 0,
            key_headers: None,
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        let decision = FaultDecision::None;
        cache.insert(key.clone(), decision);

        // Should always return None when disabled
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.size(), 0);
    }

    #[test]
    fn test_cache_clear() {
        let config = DecisionCacheConfig::default();
        let cache = DecisionCache::new(config);

        // Insert multiple entries
        for i in 0..5 {
            let key = CacheKey::new(
                "GET".to_string(),
                format!("/api/test{i}"),
                None,
                vec![],
                CacheKeyBody::Json(&json!({})),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None);
        }

        assert_eq!(cache.size(), 5);

        cache.clear();
        assert_eq!(cache.size(), 0);
    }

    #[test]
    fn test_cache_hit_rate() {
        let config = DecisionCacheConfig::default();
        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        );

        // 1 miss
        cache.get(&key);

        cache.insert(key.clone(), FaultDecision::None);

        // 3 hits
        cache.get(&key);
        cache.get(&key);
        cache.get(&key);

        let metrics = cache.metrics();
        assert_eq!(metrics.hits, 3);
        assert_eq!(metrics.misses, 1);
        assert_eq!(metrics.hit_rate(), 0.75); // 3 / (3 + 1)
    }

    #[test]
    fn test_cache_cleanup_expired() {
        let config = DecisionCacheConfig {
            enabled: true,
            max_size: 100,
            ttl_seconds: 1,
            key_headers: None,
        };

        let cache = DecisionCache::new(config);

        // Insert entries
        for i in 0..5 {
            let key = CacheKey::new(
                "GET".to_string(),
                format!("/api/test{i}"),
                None,
                vec![],
                CacheKeyBody::Json(&json!({})),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None);
        }

        assert_eq!(cache.size(), 5);

        // Wait for expiration
        thread::sleep(Duration::from_secs(2));

        // Cleanup
        cache.cleanup_expired();

        assert_eq!(cache.size(), 0);

        let metrics = cache.metrics();
        assert_eq!(metrics.expirations, 5);
    }

    fn key_n(i: usize) -> CacheKey {
        CacheKey::new(
            "GET".to_string(),
            format!("/api/test{i}"),
            None,
            vec![],
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        )
    }

    /// `max_size: 0` means the cache is off, not "hold one entry" (issue #544). The old
    /// HashMap path treated it as capacity 1 — `len() >= 0` is always true, so it evicted from an
    /// empty map and then inserted anyway.
    #[test]
    fn max_size_zero_disables_cache() {
        let cache = DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: 0,
            ttl_seconds: 0,
            key_headers: None,
        });

        cache.insert(key_n(0), FaultDecision::None);

        assert_eq!(cache.size(), 0, "max_size 0 must store nothing");
        assert!(
            cache.get(&key_n(0)).is_none(),
            "max_size 0 must never serve a hit"
        );
    }

    #[test]
    fn max_size_one_keeps_only_the_newest() {
        let cache = DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: 1,
            ttl_seconds: 0,
            key_headers: None,
        });

        cache.insert(key_n(0), FaultDecision::None);
        cache.insert(key_n(1), FaultDecision::None);

        assert_eq!(cache.size(), 1);
        assert!(cache.get(&key_n(0)).is_none(), "oldest must be evicted");
        assert!(cache.get(&key_n(1)).is_some(), "newest must be retained");
    }

    /// The cache must bound itself at `max_size` and count exactly the overflow as evictions.
    #[test]
    fn capacity_invariant_holds_beyond_max_size() {
        const MAX: usize = 8;
        const OVERFLOW: usize = 5;

        let cache = DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: MAX,
            ttl_seconds: 0,
            key_headers: None,
        });

        for i in 0..(MAX + OVERFLOW) {
            cache.insert(key_n(i), FaultDecision::None);
        }

        assert_eq!(cache.size(), MAX, "cache must stay bounded at max_size");
        let metrics = cache.metrics();
        assert_eq!(
            metrics.evictions, OVERFLOW as u64,
            "exactly the overflow must be evicted"
        );
        assert_eq!(metrics.inserts, (MAX + OVERFLOW) as u64);
        assert_eq!(metrics.size, MAX, "reported size must track the real size");
    }

    #[test]
    fn reinserting_existing_key_at_capacity_does_not_evict() {
        const MAX: usize = 4;

        let cache = DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: MAX,
            ttl_seconds: 0,
            key_headers: None,
        });

        for i in 0..MAX {
            cache.insert(key_n(i), FaultDecision::None);
        }
        assert_eq!(cache.metrics().evictions, 0);

        // Replacing a key that is already present displaces nothing.
        cache.insert(key_n(MAX - 1), FaultDecision::None);

        assert_eq!(cache.size(), MAX);
        assert_eq!(
            cache.metrics().evictions,
            0,
            "replacing an existing key must not evict another"
        );

        // Re-inserting must also promote: key 0 is now the oldest, so the next insert evicts it
        // rather than the key we just refreshed.
        cache.insert(key_n(MAX), FaultDecision::None);
        assert!(
            cache.get(&key_n(0)).is_none(),
            "the least-recently-used key must be the victim"
        );
        assert!(
            cache.get(&key_n(MAX - 1)).is_some(),
            "a re-inserted key must be promoted, not left as the next victim"
        );
    }

    /// Concurrent mixed traffic must not deadlock or panic — the cache is one `Arc` shared across
    /// every tokio worker of a ProxyServer.
    #[test]
    fn concurrent_get_and_insert_do_not_deadlock() {
        use std::sync::Arc;

        let cache = Arc::new(DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: 16,
            ttl_seconds: 0,
            key_headers: None,
        }));

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let k = key_n((t * 200 + i) % 32);
                        cache.insert(k.clone(), FaultDecision::None);
                        let _ = cache.get(&k);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().expect("no worker may panic");
        }

        assert!(cache.size() <= 16, "cache must stay bounded under races");
    }

    fn cfg_with_key_headers(key_headers: Option<Vec<String>>) -> DecisionCache {
        DecisionCache::new(DecisionCacheConfig {
            enabled: true,
            max_size: 16,
            ttl_seconds: 0,
            key_headers,
        })
    }

    fn headers_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn key_from(cache: &DecisionCache, headers: &HashMap<String, String>) -> CacheKey {
        CacheKey::new(
            "GET".to_string(),
            "/api".to_string(),
            None,
            cache.key_headers(headers),
            CacheKeyBody::Json(&json!({})),
            "rule1".to_string(),
        )
    }

    /// Default (`None`) must key on every header — byte-identical to pre-#630 behaviour.
    #[test]
    fn key_headers_none_keys_on_every_header() {
        let cache = cfg_with_key_headers(None);
        let a = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t1"), ("x-request-id", "r1")]),
        );
        let b = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t1"), ("x-request-id", "r2")]),
        );
        assert_ne!(
            a, b,
            "with no allowlist every header participates, so a differing x-request-id must split the key"
        );
    }

    /// The whole point of #630: a per-request-unique header must not split the key once the user
    /// has declared what their scripts actually read.
    #[test]
    fn key_headers_allowlist_ignores_unlisted_headers() {
        let cache = cfg_with_key_headers(Some(vec!["x-tenant".to_string()]));
        let a = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t1"), ("x-request-id", "r1")]),
        );
        let b = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t1"), ("x-request-id", "r2")]),
        );
        assert_eq!(a, b, "an unlisted header must not participate in the key");
    }

    #[test]
    fn key_headers_allowlist_still_splits_on_listed_headers() {
        let cache = cfg_with_key_headers(Some(vec!["x-tenant".to_string()]));
        let a = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t1"), ("x-request-id", "r1")]),
        );
        let b = key_from(
            &cache,
            &headers_of(&[("x-tenant", "t2"), ("x-request-id", "r1")]),
        );
        assert_ne!(a, b, "a listed header must still split the key");
    }

    /// Config is written by humans (`X-Tenant`); hyper lowercases the wire name (`x-tenant`).
    #[test]
    fn key_headers_allowlist_is_case_insensitive() {
        let cache = cfg_with_key_headers(Some(vec!["X-Tenant".to_string()]));
        let kept = cache.key_headers(&headers_of(&[("x-tenant", "t1"), ("x-request-id", "r1")]));
        assert_eq!(
            kept,
            vec![("x-tenant".to_string(), "t1".to_string())],
            "a config-cased allowlist entry must match the lowercased wire header"
        );
    }

    /// An empty allowlist is a legitimate declaration: "no header affects my decisions".
    #[test]
    fn key_headers_empty_allowlist_drops_all_headers() {
        let cache = cfg_with_key_headers(Some(vec![]));
        assert!(
            cache
                .key_headers(&headers_of(&[("x-tenant", "t1")]))
                .is_empty(),
            "an empty allowlist keys on no headers at all"
        );
    }

    #[test]
    fn degenerate_cache_is_detected_only_after_enough_lookups_and_below_the_floor() {
        // Too few lookups to judge, even at 0%.
        assert!(!is_degenerate(0, DEGENERATE_MIN_LOOKUPS - 1));
        // ~0% hit rate over a meaningful sample: the #630 condition.
        assert!(is_degenerate(0, DEGENERATE_MIN_LOOKUPS));
        assert!(is_degenerate(1, DEGENERATE_MIN_LOOKUPS * 100));
        // A healthy cache must never trip it.
        assert!(!is_degenerate(
            DEGENERATE_MIN_LOOKUPS,
            DEGENERATE_MIN_LOOKUPS
        ));
        assert!(!is_degenerate(50, 50));
    }

    // ===== The body's participation in the key (issue #652) =====
    //
    // The memoised function is a user script that reads the body as `ctx.request.raw_body`, so the
    // key must cover the bytes. It used to cover only the *parsed* Value, and every non-JSON body
    // parsed to `Null` — so two different uploads shared one entry and the second was served the
    // first's fault decision, silently.

    fn key_with(body: CacheKeyBody<'_>) -> CacheKey {
        CacheKey::new(
            "POST".to_string(),
            "/api/upload".to_string(),
            None,
            vec![(
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body,
            "rule1".to_string(),
        )
    }

    // ===== The query's participation in the key (issue #660) =====
    //
    // Same invariant as #652, next component over: the memoised script reads `ctx.request.query`,
    // so the key must cover it. It covered only `uri.path()`, which excludes the query — so
    // `?page=1` and `?page=2` shared one entry and the second was served the first's decision.

    fn key_with_query(query: Option<&str>) -> CacheKey {
        CacheKey::new(
            "GET".to_string(),
            "/api/widgets".to_string(),
            query,
            vec![("accept".to_string(), "application/json".to_string())],
            CacheKeyBody::Raw(b""),
            "rule1".to_string(),
        )
    }

    /// The reported defect: everything equal but the query value.
    #[test]
    fn different_query_values_get_different_keys() {
        assert_ne!(
            key_with_query(Some("page=1")),
            key_with_query(Some("page=2")),
            "two different queries must not share a cached script decision"
        );
    }

    /// The cache must still cache.
    #[test]
    fn identical_queries_share_a_key() {
        assert_eq!(
            key_with_query(Some("page=1&sort=asc")),
            key_with_query(Some("page=1&sort=asc")),
            "the same query must still hit the same entry"
        );
    }

    /// No query at all is not the same request as an empty query (`/path` vs `/path?`), so they
    /// must not share an entry.
    ///
    /// Unlike the body tag, which is load-bearing (`Value::Null` and `b""` genuinely collide
    /// untagged), this one is belt-and-braces: `Hash for str` appends a `0xff` terminator, so
    /// `Some("")` already differs from hashing nothing. Verified by mutation — this test still
    /// passes with `QUERY_TAG`/`NO_QUERY_TAG` removed. The tag stays because it makes the property
    /// structural rather than a consequence of a `str` hashing detail, but do not mistake this
    /// test for a proof of it.
    #[test]
    fn absent_query_and_empty_query_are_different_keys() {
        assert_ne!(
            key_with_query(None),
            key_with_query(Some("")),
            "an absent query and an empty one are different requests"
        );
    }

    /// Keying on the RAW spelling means two orderings of the same logical query are two entries.
    /// That is deliberate (issue #660): it can only cost hit rate, never correctness, whereas
    /// keying on the parsed map would under-split the day a script gains a raw-query accessor.
    /// Pinned so a future "canonicalise the query" optimisation trips a test, not a reviewer.
    #[test]
    fn reordered_query_over_splits_deliberately() {
        assert_ne!(
            key_with_query(Some("a=1&b=2")),
            key_with_query(Some("b=2&a=1")),
            "raw-spelling keying over-splits on purpose — the safe direction"
        );
    }

    /// The query hash is stored in the key and compared by `Eq`, so — exactly like `hash_body`
    /// (issue #654) — its seed must be fixed, or the same query would hash differently per call
    /// and the cache would never hit again while every other test stayed green.
    #[test]
    fn hash_query_is_stable_across_calls() {
        assert_eq!(
            CacheKey::hash_query(Some("page=1")),
            CacheKey::hash_query(Some("page=1"))
        );
        assert_eq!(CacheKey::hash_query(None), CacheKey::hash_query(None));
    }

    /// The reported bug, as a regression test: the exact probe from issue #652.
    #[test]
    fn different_binary_bodies_get_different_keys() {
        let a = b"\x00\x01PROTOBUF-PAYLOAD-A";
        let b = b"\xff\xfeTOTALLY-DIFFERENT-B";
        assert_ne!(
            key_with(CacheKeyBody::Raw(a)),
            key_with(CacheKeyBody::Raw(b)),
            "two different binary bodies must not share a cached script decision"
        );
    }

    #[test]
    fn different_text_bodies_get_different_keys() {
        assert_ne!(
            key_with(CacheKeyBody::Raw(b"hello world")),
            key_with(CacheKeyBody::Raw(b"goodbye world")),
            "two different text bodies must not share a cached script decision"
        );
    }

    /// The cache must still cache: an identical retried payload keeps hitting.
    #[test]
    fn identical_non_json_bodies_share_a_key() {
        assert_eq!(
            key_with(CacheKeyBody::Raw(b"\x00\x01PROTOBUF-PAYLOAD-A")),
            key_with(CacheKeyBody::Raw(b"\x00\x01PROTOBUF-PAYLOAD-A")),
            "the same bytes must still hit the same entry"
        );
    }

    /// The domain tag: a JSON value and raw bytes are hashed in separate domains, so a literal
    /// `null` body, an empty body, and a non-JSON body are three distinct keys — the three that
    /// all collapsed onto one hash before.
    #[test]
    fn json_null_empty_and_raw_bodies_are_three_distinct_keys() {
        let json_null = key_with(CacheKeyBody::Json(&serde_json::Value::Null));
        let empty = key_with(CacheKeyBody::Raw(b""));
        let raw = key_with(CacheKeyBody::Raw(b"\x00\x01binary"));

        assert_ne!(json_null, empty, "a literal JSON null is not an empty body");
        assert_ne!(json_null, raw, "a literal JSON null is not a binary body");
        assert_ne!(empty, raw, "an empty body is not a binary body");
    }

    /// The body hash is **stored** in the key (`body_hash: u64`) and compared by `Eq`, so it must
    /// be stable across calls for the life of the process — a per-call random seed would give the
    /// same body a different hash every request, and the cache would never hit again while every
    /// correctness test still passed. That is why `hash_body` seeds with `foldhash`'s `FixedState`
    /// and not `RandomState` (issue #654); the LRU map's own hasher is a different question, and
    /// is randomly seeded on purpose.
    #[test]
    fn hash_body_is_stable_across_calls() {
        let body = serde_json::json!({"order": {"id": 7, "items": [1, 2, 3]}});
        assert_eq!(
            CacheKey::hash_body(CacheKeyBody::Json(&body)),
            CacheKey::hash_body(CacheKeyBody::Json(&body)),
            "a stored key hash seeded per call would never match itself again"
        );
        assert_eq!(
            CacheKey::hash_body(CacheKeyBody::Raw(b"\x00\x01binary")),
            CacheKey::hash_body(CacheKeyBody::Raw(b"\x00\x01binary")),
            "the raw arm must be just as stable as the json arm"
        );
    }

    /// A raw body whose bytes are the JSON text of a value must not collide with that parsed value:
    /// the tag separates the domains even when the payload is byte-identical.
    #[test]
    fn raw_bytes_do_not_collide_with_the_json_they_spell() {
        let value = serde_json::json!({"a": 1});
        assert_ne!(
            key_with(CacheKeyBody::Json(&value)),
            key_with(CacheKeyBody::Raw(br#"{"a":1}"#)),
            "the domain tag must keep parsed JSON and raw bytes apart"
        );
    }

    /// #653's invariant survives: JSON is keyed structurally, so formatting and key order — which
    /// the script cannot observe through `ctx.request.body` — do not split the key.
    #[test]
    fn structurally_equal_json_still_shares_a_key() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":[2,3]}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str("{ \"b\" : [2, 3], \"a\" : 1 }").unwrap();
        assert_eq!(
            key_with(CacheKeyBody::Json(&a)),
            key_with(CacheKeyBody::Json(&b)),
            "structurally equal JSON must keep sharing one entry (issue #653)"
        );
    }

    /// The body is only one component: the rest of the key still separates entries, and a raw body
    /// does not accidentally swallow them.
    #[test]
    fn raw_body_keys_still_separate_on_the_other_components() {
        let base = key_with(CacheKeyBody::Raw(b"same-bytes"));
        let other_rule = CacheKey::new(
            "POST".to_string(),
            "/api/upload".to_string(),
            None,
            vec![(
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            )],
            CacheKeyBody::Raw(b"same-bytes"),
            "rule2".to_string(),
        );
        assert_ne!(base, other_rule, "rule_id still separates identical bodies");
    }
}
