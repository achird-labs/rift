//! Recording configuration for proxy record/replay.

use crate::recording::ProxyMode;
use serde::{Deserialize, Serialize};

/// Recording configuration for proxy record/replay (Mountebank-compatible)
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecordingConfig {
    /// Recording mode: proxyOnce, proxyAlways, or proxyTransparent (default)
    #[serde(default)]
    pub mode: ProxyMode,

    /// Capture actual response latency in recorded response (Mountebank addWaitBehavior)
    #[serde(default)]
    pub add_wait_behavior: bool,

    /// Auto-generate stubs from recorded requests (Mountebank predicateGenerators)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predicate_generators: Vec<PredicateGenerator>,

    /// Persistence configuration for recordings
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<RecordingPersistence>,
}

/// Predicate generator for auto-generating stubs from recorded requests
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PredicateGenerator {
    /// Which request fields to match on
    #[serde(default)]
    pub matches: PredicateGeneratorMatches,
}

/// Fields to match on when generating predicates
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PredicateGeneratorMatches {
    /// Include method in predicate
    #[serde(default)]
    pub method: bool,
    /// Include path in predicate
    #[serde(default)]
    pub path: bool,
    /// Include query parameters in predicate
    #[serde(default)]
    pub query: bool,
    /// Include specific headers in predicate
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
}

/// Persistence configuration for recordings
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingPersistence {
    /// Persistence type: "file" or "redis"
    #[serde(default = "default_persistence_type")]
    pub backend: String,
    /// File path for file-based persistence
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Redis URL for Redis-based persistence
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_url: Option<String>,
}

fn default_persistence_type() -> String {
    "file".to_string()
}
