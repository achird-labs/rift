//! Listen, metrics, and TLS configuration.

use super::protocol::Protocol;
use serde::{Deserialize, Serialize};

/// TLS configuration for HTTPS listener
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to TLS certificate file (PEM format)
    pub cert_path: String,
    /// Path to TLS private key file (PEM format)
    pub key_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListenConfig {
    pub port: u16,
    /// Number of worker threads (0 = auto-detect CPU count)
    #[serde(default)]
    pub workers: usize,
    /// Protocol for listener (http or https)
    #[serde(default)]
    pub protocol: Protocol,
    /// TLS configuration (required when protocol is https)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_port")]
    pub port: u16,
}

fn default_metrics_port() -> u16 {
    9090
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            port: default_metrics_port(),
        }
    }
}
