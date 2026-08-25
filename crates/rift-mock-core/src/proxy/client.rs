//! HTTP client creation and configuration.
//!
//! This module provides functionality for creating and configuring
//! the shared HTTP client used for proxying requests.

use super::outbound_tls::OutboundTls;
use crate::config::Config;
use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;
use tracing::info;

/// Type alias for the HTTP client used by the proxy.
pub type HttpClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    BoxBody<Bytes, hyper::Error>,
>;

/// Create a shared HTTP client with connection pooling.
///
/// # Arguments
/// * `config` - The proxy configuration
/// * `policy` - The process-wide outbound TLS trust policy (issue #974). A per-upstream
///   `tls_skip_verify` in `config` widens it, so both spellings keep working.
///
/// # Returns
/// A configured HTTP client ready for proxying requests, or an error if the trust policy cannot be
/// realised — e.g. an unparsable CA PEM, or a minimal/distroless image with no `ca-certificates`
/// and no supplied anchor. Returned rather than panicked so the caller can fail gracefully
/// (issue #543).
pub fn create_http_client(config: &Config, policy: &OutboundTls) -> anyhow::Result<HttpClient> {
    // Create HTTP connector with connection pool settings
    let mut http_connector = hyper_util::client::legacy::connect::HttpConnector::new();
    http_connector.set_keepalive(Some(Duration::from_secs(
        config.connection_pool.keepalive_timeout_secs,
    )));
    http_connector.set_connect_timeout(Some(Duration::from_secs(
        config.connection_pool.connect_timeout_secs,
    )));
    http_connector.enforce_http(false); // Allow both HTTP and HTTPS

    // A per-upstream `tls_skip_verify` can only ever *widen* the process policy: an operator who
    // marked one upstream insecure must not thereby tighten the rest, nor lose a configured CA.
    let effective = OutboundTls {
        ca_pem: policy.ca_pem.clone(),
        skip_verify: policy.skip_verify || should_skip_tls_verify(config),
    };

    let https_connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(effective.client_config()?)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http_connector);

    let http_client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(
            config.connection_pool.idle_timeout_secs,
        ))
        .pool_max_idle_per_host(config.connection_pool.max_idle_per_host)
        .build(https_connector);

    info!(
        "Connection pool configured (HTTP/1.1): max_idle={}, idle_timeout={}s, keepalive={}s",
        config.connection_pool.max_idle_per_host,
        config.connection_pool.idle_timeout_secs,
        config.connection_pool.keepalive_timeout_secs
    );

    Ok(http_client)
}

/// Check if any upstream needs TLS verification skipped.
pub fn should_skip_tls_verify(config: &Config) -> bool {
    config.upstreams.iter().any(|u| u.tls_skip_verify)
        || config
            .upstream
            .as_ref()
            .map(|u| u.tls_skip_verify)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> Config {
        serde_json::from_value(serde_json::json!({ "listen": { "port": 0 } }))
            .expect("minimal config deserializes")
    }

    #[test]
    fn create_http_client_returns_result_ok_for_both_tls_modes() {
        let config = minimal_config();
        // Fix for issue #543: the fn returns a Result. On a normal host with a CA bundle both
        // paths succeed; the point is that a native-root load failure is a returned Err, never a
        // panic that aborts server construction.
        assert!(create_http_client(&config, &OutboundTls::default()).is_ok());
        // The skip-verify path never touches the native root store, so it cannot fail on it.
        assert!(
            create_http_client(
                &config,
                &OutboundTls {
                    ca_pem: None,
                    skip_verify: true,
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn create_http_client_surfaces_a_bad_ca_pem() {
        // Issue #974: a malformed anchor is a returned error here, not a client that quietly
        // trusts only the OS store — the operator asked for an extra anchor and must be told.
        let config = minimal_config();
        let policy = OutboundTls {
            ca_pem: Some("not a pem".to_string()),
            skip_verify: false,
        };
        assert!(create_http_client(&config, &policy).is_err());
    }
}
