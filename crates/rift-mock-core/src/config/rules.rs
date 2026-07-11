//! Fault injection rules configuration.

use crate::behaviors::ResponseBehaviors;
use crate::predicate::{BodyMatcher, HeaderMatcher, QueryMatcher};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Rule {
    pub id: String,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    pub fault: FaultConfig,
    // Optional: scope fault to specific upstream (v3 multi-upstream mode)
    // If None, applies to all upstreams
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MatchConfig {
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path: PathMatch,
    /// Simple header matching (backward compatible)
    #[serde(default)]
    pub headers: Vec<super::routing::HeaderMatch>,

    // ===== Enhanced Mountebank-compatible predicates =====
    /// Enhanced header matching with operators (contains, startsWith, etc.)
    /// Use this OR headers, not both
    #[serde(
        default,
        rename = "headerPredicates",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub header_predicates: Vec<HeaderMatcher>,

    /// Query parameter matching
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<QueryMatcher>,

    /// Request body matching
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyMatcher>,

    /// Case-sensitive matching (default: true)
    #[serde(default = "default_case_sensitive", rename = "caseSensitive")]
    pub case_sensitive: bool,
}

fn default_case_sensitive() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(untagged)]
pub enum PathMatch {
    #[default]
    Any,
    Exact {
        exact: String,
    },
    Prefix {
        prefix: String,
    },
    Regex {
        regex: String,
    },
    /// Path contains substring (Mountebank-compatible)
    Contains {
        contains: String,
    },
    /// Path ends with suffix (Mountebank-compatible)
    EndsWith {
        #[serde(rename = "endsWith")]
        ends_with: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FaultConfig {
    #[serde(default)]
    pub latency: Option<LatencyFault>,
    #[serde(default)]
    pub error: Option<ErrorFault>,
    /// TCP-level fault (Mountebank-compatible)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_fault: Option<TcpFault>,
}

/// TCP-level fault types (Mountebank-compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TcpFault {
    /// Immediately close TCP connection with RST
    ConnectionResetByPeer,
    /// Send random garbage data then close
    RandomDataThenClose,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LatencyFault {
    pub probability: f64,
    pub min_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorFault {
    pub probability: f64,
    pub status: u16,
    #[serde(default)]
    pub body: String,
    /// Optional headers to include in error response (can be overridden by script headers)
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
    /// Mountebank-compatible response behaviors (wait, repeat, copy, lookup)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behaviors: Option<ResponseBehaviors>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptRule {
    pub id: String,
    pub script: String, // inline script or path to script file
    #[serde(default, rename = "match")]
    pub match_config: MatchConfig,
    // Optional: scope fault to specific upstream (v3 multi-upstream mode)
    // If None, applies to all upstreams
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}
