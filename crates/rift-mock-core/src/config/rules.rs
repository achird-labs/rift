//! Fault-injection configuration.
//!
//! The rule/matcher types that used to live here (`Rule`, `MatchConfig`, `PathMatch`,
//! `ScriptRule`) belonged to the reverse-proxy `Config` mode and were removed with it in #975.
//! What remains is the fault vocabulary reached through [`crate::extensions::fault`]. Note it has
//! **no in-tree reader**: the imposter path injects faults through its own `_rift.fault` type
//! (`imposter::types::RiftFaultConfig`), which shares no code with these. Retained as public API
//! pending its own decision, not because anything here consumes it.

use crate::behaviors::ResponseBehaviors;
use serde::{Deserialize, Serialize};

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
