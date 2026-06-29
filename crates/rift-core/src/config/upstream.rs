//! Upstream and connection pool configuration.

use super::protocol::Protocol;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub host: String,
    pub port: u16,
    /// Protocol: http or https (default: http)
    /// Note: 'scheme' is deprecated but maintained for backward compatibility
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// Deprecated: use 'protocol' instead
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Skip TLS certificate verification (for self-signed certs in dev/test)
    #[serde(default)]
    pub tls_skip_verify: bool,
}

impl UpstreamConfig {
    /// Get the protocol, checking both new 'protocol' field and legacy 'scheme' field
    pub fn get_protocol(&self) -> Protocol {
        // Prefer new 'protocol' field
        if let Some(protocol) = self.protocol {
            return protocol;
        }

        // Fall back to legacy 'scheme' field
        if let Some(ref scheme) = self.scheme {
            Protocol::from_scheme(scheme).unwrap_or(Protocol::Http)
        } else {
            Protocol::Http
        }
    }
}

/// Named upstream for reverse proxy mode
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Upstream {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// Skip TLS certificate verification (for self-signed certs in dev/test)
    #[serde(default)]
    pub tls_skip_verify: bool,
}

impl Upstream {
    /// Parse and extract protocol from URL
    /// Returns the protocol or an error if URL is invalid or protocol is unsupported
    pub fn get_protocol(&self) -> Result<Protocol, String> {
        // Parse URL to extract scheme
        let url_parts: Vec<&str> = self.url.splitn(2, "://").collect();
        if url_parts.len() != 2 {
            return Err(format!("Invalid URL format (missing scheme): {}", self.url));
        }

        Protocol::from_scheme(url_parts[0])
    }

    /// Validate that the upstream configuration is valid
    pub fn validate(&self) -> Result<(), String> {
        // Check protocol is valid and supported
        let protocol = self.get_protocol()?;
        if !protocol.is_supported() {
            return Err(format!(
                "Unsupported protocol '{}' for upstream '{}'. Currently supported: http, https",
                protocol.as_str(),
                self.name
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthCheckConfig {
    #[serde(default = "default_health_path")]
    pub path: String,
    #[serde(default = "default_health_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_health_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_health_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    #[serde(default = "default_health_healthy_threshold")]
    pub healthy_threshold: u32,
}

fn default_health_path() -> String {
    "/health".to_string()
}

fn default_health_interval() -> u64 {
    30
}

fn default_health_timeout() -> u64 {
    5
}

fn default_health_unhealthy_threshold() -> u32 {
    3
}

fn default_health_healthy_threshold() -> u32 {
    2
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            path: default_health_path(),
            interval_seconds: default_health_interval(),
            timeout_seconds: default_health_timeout(),
            unhealthy_threshold: default_health_unhealthy_threshold(),
            healthy_threshold: default_health_healthy_threshold(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConnectionPoolConfig {
    #[serde(default = "default_pool_max_idle_per_host")]
    pub max_idle_per_host: usize,

    #[serde(default = "default_pool_idle_timeout")]
    pub idle_timeout_secs: u64,

    #[serde(default = "default_keepalive_timeout")]
    pub keepalive_timeout_secs: u64,

    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
}

impl Default for ConnectionPoolConfig {
    fn default() -> Self {
        Self {
            max_idle_per_host: default_pool_max_idle_per_host(),
            idle_timeout_secs: default_pool_idle_timeout(),
            keepalive_timeout_secs: default_keepalive_timeout(),
            connect_timeout_secs: default_connect_timeout(),
        }
    }
}

fn default_pool_max_idle_per_host() -> usize {
    100
}

fn default_pool_idle_timeout() -> u64 {
    90
}

fn default_keepalive_timeout() -> u64 {
    60
}

fn default_connect_timeout() -> u64 {
    5
}
