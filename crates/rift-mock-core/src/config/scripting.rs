//! Flow-state (scenario state) configuration.
//!
//! `ScriptEngineConfig`, `ScriptPoolConfigFile` and `DecisionCacheConfigFile` were fields of the
//! reverse-proxy `Config` and had no other reader; they went with it in #975.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlowStateConfig {
    #[serde(default = "default_backend_type")]
    pub backend: String, // "inmemory"
    #[serde(default = "default_ttl_seconds")]
    pub ttl_seconds: i64,
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
        }
    }
}
