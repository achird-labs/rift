use crate::scripting::FaultDecision;
use anyhow::Result;
use lru::LruCache;
use parking_lot::Mutex;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// Configuration for the decision cache
#[derive(Clone, Debug)]

pub struct DecisionCacheConfig {
    /// Enable decision caching
    pub enabled: bool,
    /// Maximum number of cache entries (LRU eviction when exceeded)
    pub max_size: usize,
    /// TTL for cache entries in seconds (0 = no expiration)
    pub ttl_seconds: u64,
}

impl Default for DecisionCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: 10000,
            ttl_seconds: 300, // 5 minutes
        }
    }
}

/// Cache key derived from request properties
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    /// Request method
    method: String,
    /// Request path
    path: String,
    /// Sorted header keys and values (for deterministic hashing)
    headers: Vec<(String, String)>,
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
        mut headers: Vec<(String, String)>,
        body: &serde_json::Value,
        rule_id: String,
    ) -> Self {
        // Sort headers for deterministic key generation
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        // Hash the body to avoid storing large payloads
        let body_hash = Self::hash_json(body);

        Self {
            method,
            path,
            headers,
            body_hash,
            rule_id,
        }
    }

    /// Hash a JSON value for cache key generation
    fn hash_json(value: &serde_json::Value) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // Use canonical JSON string for consistent hashing
        let json_str = serde_json::to_string(value).unwrap_or_default();
        json_str.hash(&mut hasher);
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
    cache: Option<Mutex<LruCache<CacheKey, CacheEntry>>>,
    metrics: AtomicMetrics,
}

impl DecisionCache {
    /// Create a new decision cache
    pub fn new(config: DecisionCacheConfig) -> Self {
        debug!(
            "Creating decision cache: enabled={}, max_size={}, ttl={}s",
            config.enabled, config.max_size, config.ttl_seconds
        );

        let cache = match (config.enabled, NonZeroUsize::new(config.max_size)) {
            (true, Some(capacity)) => Some(Mutex::new(LruCache::new(capacity))),
            _ => None,
        };

        Self {
            config,
            cache,
            metrics: AtomicMetrics::default(),
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
                self.metrics.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert a decision into the cache
    pub fn insert(&self, key: CacheKey, decision: FaultDecision) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
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

        Ok(())
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

    #[test]
    fn test_cache_key_creation() {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-request-id".to_string(), "123".to_string()),
        ];

        let key1 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            headers.clone(),
            &json!({"foo": "bar"}),
            "rule1".to_string(),
        );

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            headers.clone(),
            &json!({"foo": "bar"}),
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
            headers1,
            &json!({}),
            "rule1".to_string(),
        );

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            headers2,
            &json!({}),
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
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            vec![],
            &json!({}),
            "rule1".to_string(),
        );

        // Cache miss
        assert!(cache.get(&key).is_none());

        // Insert
        let decision = FaultDecision::Latency {
            duration_ms: 100,
            rule_id: "rule1".to_string(),
        };
        cache.insert(key.clone(), decision.clone()).unwrap();

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
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            vec![],
            &json!({}),
            "rule1".to_string(),
        );

        let decision = FaultDecision::None;
        cache.insert(key.clone(), decision).unwrap();

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
        };

        let cache = DecisionCache::new(config);

        // Insert 3 entries
        for i in 0..3 {
            let key = CacheKey::new(
                "GET".to_string(),
                format!("/api/test{i}"),
                vec![],
                &json!({}),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None).unwrap();
        }

        assert_eq!(cache.size(), 3);

        // Access key 1 and 2 to make key 0 the LRU
        let key1 = CacheKey::new(
            "GET".to_string(),
            "/api/test1".to_string(),
            vec![],
            &json!({}),
            "rule1".to_string(),
        );
        cache.get(&key1);

        let key2 = CacheKey::new(
            "GET".to_string(),
            "/api/test2".to_string(),
            vec![],
            &json!({}),
            "rule2".to_string(),
        );
        cache.get(&key2);

        // Insert 4th entry - should evict key 0 (LRU)
        let key3 = CacheKey::new(
            "GET".to_string(),
            "/api/test3".to_string(),
            vec![],
            &json!({}),
            "rule3".to_string(),
        );
        cache.insert(key3, FaultDecision::None).unwrap();

        assert_eq!(cache.size(), 3);

        // Key 0 should be evicted
        let key0 = CacheKey::new(
            "GET".to_string(),
            "/api/test0".to_string(),
            vec![],
            &json!({}),
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
        };

        let cache = DecisionCache::new(config);

        let key = CacheKey::new(
            "GET".to_string(),
            "/api/test".to_string(),
            vec![],
            &json!({}),
            "rule1".to_string(),
        );

        let decision = FaultDecision::None;
        cache.insert(key.clone(), decision).unwrap();

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
                vec![],
                &json!({}),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None).unwrap();
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
            vec![],
            &json!({}),
            "rule1".to_string(),
        );

        // 1 miss
        cache.get(&key);

        cache.insert(key.clone(), FaultDecision::None).unwrap();

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
        };

        let cache = DecisionCache::new(config);

        // Insert entries
        for i in 0..5 {
            let key = CacheKey::new(
                "GET".to_string(),
                format!("/api/test{i}"),
                vec![],
                &json!({}),
                format!("rule{i}"),
            );
            cache.insert(key, FaultDecision::None).unwrap();
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
            vec![],
            &json!({}),
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
        });

        cache.insert(key_n(0), FaultDecision::None).unwrap();

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
        });

        cache.insert(key_n(0), FaultDecision::None).unwrap();
        cache.insert(key_n(1), FaultDecision::None).unwrap();

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
        });

        for i in 0..(MAX + OVERFLOW) {
            cache.insert(key_n(i), FaultDecision::None).unwrap();
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
        });

        for i in 0..MAX {
            cache.insert(key_n(i), FaultDecision::None).unwrap();
        }
        assert_eq!(cache.metrics().evictions, 0);

        // Replacing a key that is already present displaces nothing.
        cache.insert(key_n(MAX - 1), FaultDecision::None).unwrap();

        assert_eq!(cache.size(), MAX);
        assert_eq!(
            cache.metrics().evictions,
            0,
            "replacing an existing key must not evict another"
        );

        // Re-inserting must also promote: key 0 is now the oldest, so the next insert evicts it
        // rather than the key we just refreshed.
        cache.insert(key_n(MAX), FaultDecision::None).unwrap();
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
        }));

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let k = key_n((t * 200 + i) % 32);
                        cache.insert(k.clone(), FaultDecision::None).unwrap();
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
}
