//! Protocol and deployment mode types.

use serde::{Deserialize, Serialize};

/// Protocol supported by Rift for listeners and upstreams
/// Extensible design to support future protocols (TCP, WebSocket, DynamoDB, etc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// HTTP protocol
    #[default]
    Http,
    /// HTTPS protocol (HTTP over TLS)
    Https,
    /// TCP protocol (for future support)
    #[serde(rename = "tcp")]
    Tcp,
    /// WebSocket protocol (for future support)
    #[serde(rename = "websocket")]
    WebSocket,
    /// DynamoDB protocol (for future support - Mountebank compatibility)
    #[serde(rename = "dynamodb")]
    DynamoDB,
}

impl Protocol {
    /// Check if protocol is currently supported
    pub fn is_supported(&self) -> bool {
        matches!(self, Protocol::Http | Protocol::Https)
    }

    /// Get protocol name as string
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Https => "https",
            Protocol::Tcp => "tcp",
            Protocol::WebSocket => "websocket",
            Protocol::DynamoDB => "dynamodb",
        }
    }

    /// Parse protocol from URL scheme
    pub fn from_scheme(scheme: &str) -> Result<Self, String> {
        match scheme.to_lowercase().as_str() {
            "http" => Ok(Protocol::Http),
            "https" => Ok(Protocol::Https),
            "tcp" => Ok(Protocol::Tcp),
            "ws" | "websocket" => Ok(Protocol::WebSocket),
            "dynamodb" => Ok(Protocol::DynamoDB),
            _ => Err(format!("Unsupported protocol scheme: {scheme}")),
        }
    }
}

/// Deployment mode for Rift proxy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentMode {
    /// Sidecar mode: single upstream target
    Sidecar,
    /// Reverse proxy mode: multiple upstreams with routing
    ReverseProxy,
}
