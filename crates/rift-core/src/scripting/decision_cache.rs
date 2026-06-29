use crate::scripting::FaultDecision;
use anyhow::Result;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};
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
#[derive(Clone, Debug)]

struct CacheEntry {
    decision: FaultDecision,
    created_at: Instant,
    last_accessed: Instant,
    access_count: u64,
}

impl CacheEntry {
    fn new(decision: FaultDecision) -> Self {
        let now = Instant::now();
        Self {
            decision,
            created_at: now,
            last_accessed: now,
            access_count: 0,
        }
    }

    fn is_expired(&self, ttl: Duration) -> bool {
        if ttl.is_zero() {
            return false; // No expiration
        }
        self.created_at.elapsed() > ttl
    }

    fn touch(&mut self) {
        self.last_accessed = Instant::now();
        self.access_count += 1;
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

/// Combined cache state protected by a single lock to prevent deadlocks.
#[derive(Debug)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    metrics: CacheMetrics,
}

/// Helper enum to avoid borrow checker issues when checking entry state
enum EntryState {
    Expired,
    Valid(FaultDecision, u64),
}

/// Decision cache for memoizing script execution results
pub struct DecisionCache {
    config: DecisionCacheConfig,
    /// Single lock protecting both cache entries and metrics to prevent deadlocks
    state: Arc<RwLock<CacheState>>,
}

impl DecisionCache {
    /// Create a new decision cache
    pub fn new(config: DecisionCacheConfig) -> Self {
        debug!(
            "Creating decision cache: enabled={}, max_size={}, ttl={}s",
            config.enabled, config.max_size, config.ttl_seconds
        );

        Self {
            config,
            state: Arc::new(RwLock::new(CacheState {
                entries: HashMap::new(),
                metrics: CacheMetrics::default(),
            })),
        }
    }

    /// Get a decision from cache if available and not expired
    pub fn get(&self, key: &CacheKey) -> Option<FaultDecision> {
        if !self.config.enabled {
            return None;
        }

        let mut state = self.state.write().expect("decision cache lock poisoned");
        let ttl = Duration::from_secs(self.config.ttl_seconds);

        // First, check if entry exists and handle expiration
        let entry_state = state.entries.get(key).map(|entry| {
            if entry.is_expired(ttl) {
                EntryState::Expired
            } else {
                EntryState::Valid(entry.decision.clone(), entry.access_count)
            }
        });

        match entry_state {
            Some(EntryState::Expired) => {
                trace!("Cache entry expired for key: {:?}", key);
                state.entries.remove(key);
                state.metrics.misses += 1;
                state.metrics.expirations += 1;
                state.metrics.size = state.entries.len();
                None
            }
            Some(EntryState::Valid(decision, access_count)) => {
                // Update the entry's access time
                if let Some(entry) = state.entries.get_mut(key) {
                    entry.touch();
                }
                trace!(
                    "Cache hit for key: {:?} (access_count: {})",
                    key,
                    access_count + 1
                );
                state.metrics.hits += 1;
                Some(decision)
            }
            None => {
                // Cache miss
                trace!("Cache miss for key: {:?}", key);
                state.metrics.misses += 1;
                None
            }
        }
    }

    /// Insert a decision into the cache
    pub fn insert(&self, key: CacheKey, decision: FaultDecision) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        let mut state = self.state.write().expect("decision cache lock poisoned");

        // Check if we need to evict entries
        if state.entries.len() >= self.config.max_size && !state.entries.contains_key(&key) {
            Self::evict_lru(&mut state);
        }

        // Insert new entry
        state.entries.insert(key.clone(), CacheEntry::new(decision));
        trace!("Cache insert for key: {:?}", key);

        state.metrics.inserts += 1;
        state.metrics.size = state.entries.len();

        Ok(())
    }

    /// Evict the least recently used entry
    ///
    /// This is a static method that takes a mutable reference to the entire
    /// CacheState, allowing atomic updates to both entries and metrics without
    /// needing to acquire a separate lock.
    fn evict_lru(state: &mut CacheState) {
        // Find entry with oldest last_accessed time
        if let Some((key_to_evict, _)) = state
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_accessed)
            .map(|(k, v)| (k.clone(), v.clone()))
        {
            state.entries.remove(&key_to_evict);
            state.metrics.evictions += 1;
            trace!("Evicted LRU entry: {:?}", key_to_evict);
        }
    }

    /// Clear all cache entries
    pub fn clear(&self) {
        let mut state = self.state.write().expect("decision cache lock poisoned");
        state.entries.clear();
        state.metrics.size = 0;
        debug!("Cache cleared");
    }

    /// Get current cache metrics
    pub fn metrics(&self) -> CacheMetrics {
        self.state
            .read()
            .expect("decision cache lock poisoned")
            .metrics
            .clone()
    }

    /// Remove expired entries (can be called periodically)
    pub fn cleanup_expired(&self) {
        if !self.config.enabled || self.config.ttl_seconds == 0 {
            return;
        }

        let mut state = self.state.write().expect("decision cache lock poisoned");
        let ttl = Duration::from_secs(self.config.ttl_seconds);

        let expired_keys: Vec<CacheKey> = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.is_expired(ttl))
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            state.entries.remove(&key);
        }

        if count > 0 {
            debug!("Cleaned up {} expired cache entries", count);
            state.metrics.expirations += count as u64;
            state.metrics.size = state.entries.len();
        }
    }

    /// Get cache size
    pub fn size(&self) -> usize {
        self.state
            .read()
            .expect("decision cache lock poisoned")
            .entries
            .len()
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
}
