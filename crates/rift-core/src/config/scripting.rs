//! Script engine and pool configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptEngineConfig {
    #[serde(default = "default_engine_type")]
    pub engine: String, // "rhai" or "lua"
}

fn default_engine_type() -> String {
    "rhai".to_string()
}

impl Default for ScriptEngineConfig {
    fn default() -> Self {
        Self {
            engine: default_engine_type(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlowStateConfig {
    #[serde(default = "default_backend_type")]
    pub backend: String, // "inmemory", "redis", "valkey"
    #[serde(default = "default_ttl_seconds")]
    pub ttl_seconds: i64,
    #[serde(default)]
    pub redis: Option<RedisConfig>,
}

fn default_backend_type() -> String {
    "inmemory".to_string()
}

fn default_ttl_seconds() -> i64 {
    300
}

impl Default for FlowStateConfig {
    fn default() -> Self {
        Self {
            backend: default_backend_type(),
            ttl_seconds: default_ttl_seconds(),
            redis: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RedisConfig {
    pub url: String,
    #[serde(default = "default_redis_pool_size")]
    pub pool_size: usize,
    #[serde(default = "default_redis_key_prefix")]
    pub key_prefix: String,
}

fn default_redis_pool_size() -> usize {
    10
}

fn default_redis_key_prefix() -> String {
    "rift:".to_string()
}

/// Script pool configuration (M9 Phase 4 optimization)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptPoolConfigFile {
    /// Number of worker threads (0 = auto-detect: num_cpus/2, min 2, max 16)
    #[serde(default = "default_script_pool_workers")]
    pub workers: usize,
    /// Maximum queue size for pending script executions
    #[serde(default = "default_script_pool_queue_size")]
    pub queue_size: usize,
    /// Timeout in milliseconds for script execution
    #[serde(default = "default_script_pool_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_script_pool_workers() -> usize {
    0
} // 0 = auto-detect

fn default_script_pool_queue_size() -> usize {
    1000
}

fn default_script_pool_timeout_ms() -> u64 {
    5000
}

impl Default for ScriptPoolConfigFile {
    fn default() -> Self {
        Self {
            workers: default_script_pool_workers(),
            queue_size: default_script_pool_queue_size(),
            timeout_ms: default_script_pool_timeout_ms(),
        }
    }
}

/// Decision cache configuration (M9 Phase 4 optimization)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionCacheConfigFile {
    /// Enable decision caching
    #[serde(default = "default_decision_cache_enabled")]
    pub enabled: bool,
    /// Maximum number of cache entries (LRU eviction when exceeded)
    #[serde(default = "default_decision_cache_max_size")]
    pub max_size: usize,
    /// TTL for cache entries in seconds (0 = no expiration)
    #[serde(default = "default_decision_cache_ttl_seconds")]
    pub ttl_seconds: u64,
}

fn default_decision_cache_enabled() -> bool {
    true
}

fn default_decision_cache_max_size() -> usize {
    10000
}

fn default_decision_cache_ttl_seconds() -> u64 {
    300
}

impl Default for DecisionCacheConfigFile {
    fn default() -> Self {
        Self {
            enabled: default_decision_cache_enabled(),
            max_size: default_decision_cache_max_size(),
            ttl_seconds: default_decision_cache_ttl_seconds(),
        }
    }
}
