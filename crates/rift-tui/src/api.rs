//! HTTP client for Rift Admin API communication

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Errors that can occur when communicating with the Admin API
#[derive(Error, Debug)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("API returned error: {message} (code: {code})")]
    Server { code: String, message: String },
    #[error("Failed to parse response: {0}")]
    Parse(String),
    #[error("Connection failed: {0}")]
    Connection(String),
}

/// Summary of an imposter for list view
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposterSummary {
    pub port: u16,
    pub protocol: String,
    pub name: Option<String>,
    #[serde(default)]
    pub number_of_requests: u64,
    #[serde(default)]
    pub stub_count: usize,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub record_requests: bool,
}

/// Full imposter details
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposterDetail {
    pub port: u16,
    pub protocol: String,
    pub name: Option<String>,
    #[serde(default)]
    pub number_of_requests: u64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub record_requests: bool,
    #[serde(default)]
    pub stubs: Vec<Stub>,
    #[serde(default)]
    pub requests: Vec<RecordedRequest>,
}

/// Stub definition
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stub {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_from: Option<String>,
    #[serde(default)]
    pub predicates: Vec<serde_json::Value>,
    #[serde(default)]
    pub responses: Vec<serde_json::Value>,
}

/// Recorded request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedRequest {
    pub request_from: Option<String>,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub query: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub timestamp: Option<String>,
}

/// Response wrapper for imposter list
#[derive(Debug, Deserialize)]
pub struct ImpostersResponse {
    pub imposters: Vec<ImposterSummary>,
}

/// Error response from API
#[derive(Debug, Deserialize)]
pub struct ErrorResponse {
    pub errors: Vec<ApiErrorDetail>,
}

#[derive(Debug, Deserialize)]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
}

/// Request body for creating an imposter
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateImposterRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub record_requests: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stubs: Vec<Stub>,
}

/// Request body for adding a stub
#[derive(Debug, Serialize)]
pub struct AddStubRequest {
    pub stub: Stub,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

/// Request body for creating a proxy imposter
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProxyImposterRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub record_requests: bool,
    pub stubs: Vec<ProxyStub>,
}

/// A stub with proxy response
#[derive(Debug, Serialize)]
pub struct ProxyStub {
    pub responses: Vec<ProxyResponse>,
}

/// Proxy response configuration
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyResponse {
    pub proxy: ProxyConfig,
}

/// Proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub to: String,
    #[serde(default = "default_proxy_mode")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predicate_generators: Vec<PredicateGenerator>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub add_wait_behavior: bool,
}

fn default_proxy_mode() -> String {
    "proxyOnce".to_string()
}

/// Predicate generator for proxy recording
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PredicateGenerator {
    pub matches: serde_json::Value,
}

/// Metrics data parsed from Prometheus format
#[derive(Debug, Clone, Default)]
pub struct MetricsData {
    pub imposter_count: usize,
    pub total_requests: u64,
    pub per_imposter: HashMap<u16, ImposterMetrics>,
}

#[derive(Debug, Clone, Default)]
pub struct ImposterMetrics {
    pub request_count: u64,
    pub requests_per_second: f64,
}

/// HTTP client for the Rift Admin API
pub struct ApiClient {
    client: Client,
    base_url: String,
}

impl ApiClient {
    /// Create a new API client
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Get the underlying HTTP client
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get the base URL
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Check if the server is healthy
    pub async fn health_check(&self) -> Result<bool, ApiError> {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => {
                if e.is_connect() {
                    Err(ApiError::Connection(format!(
                        "Cannot connect to {}",
                        self.base_url
                    )))
                } else {
                    Err(ApiError::Request(e))
                }
            }
        }
    }

    /// List all imposters
    pub async fn list_imposters(&self) -> Result<Vec<ImposterSummary>, ApiError> {
        let url = format!("{}/imposters", self.base_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        // The list payload carries stubCount/enabled directly (issue #558) — no per-imposter
        // detail fetches, so refresh is one request per tick and a transient per-imposter failure
        // can no longer be silently rendered as "Disabled / 0 stubs".
        let body: ImpostersResponse = resp.json().await?;
        Ok(body.imposters)
    }

    /// Get details for a specific imposter
    pub async fn get_imposter(&self, port: u16) -> Result<ImposterDetail, ApiError> {
        let url = format!("{}/imposters/{}", self.base_url, port);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(resp.json().await?)
    }

    /// Create a new imposter
    pub async fn create_imposter(&self, request: CreateImposterRequest) -> Result<u16, ApiError> {
        let url = format!("{}/imposters", self.base_url);
        let resp = self.client.post(&url).json(&request).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        let detail: ImposterDetail = resp.json().await?;
        Ok(detail.port)
    }

    /// Delete an imposter
    pub async fn delete_imposter(&self, port: u16) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}", self.base_url, port);
        let resp = self.client.delete(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Enable an imposter
    pub async fn enable_imposter(&self, port: u16) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/enable", self.base_url, port);
        let resp = self.client.post(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Disable an imposter
    pub async fn disable_imposter(&self, port: u16) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/disable", self.base_url, port);
        let resp = self.client.post(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Clear recorded requests
    pub async fn clear_requests(&self, port: u16) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/savedRequests", self.base_url, port);
        let resp = self.client.delete(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Get stubs for an imposter
    pub async fn get_stubs(&self, port: u16) -> Result<Vec<Stub>, ApiError> {
        let imposter = self.get_imposter(port).await?;
        Ok(imposter.stubs)
    }

    /// Add a stub to an imposter
    pub async fn add_stub(
        &self,
        port: u16,
        stub: Stub,
        index: Option<usize>,
    ) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/stubs", self.base_url, port);
        let request = AddStubRequest { stub, index };
        let resp = self.client.post(&url).json(&request).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Update a stub
    pub async fn update_stub(&self, port: u16, index: usize, stub: Stub) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/stubs/{}", self.base_url, port, index);
        let resp = self.client.put(&url).json(&stub).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Delete a stub
    pub async fn delete_stub(&self, port: u16, index: usize) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/stubs/{}", self.base_url, port, index);
        let resp = self.client.delete(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Create a proxy imposter for recording
    pub async fn create_proxy_imposter(
        &self,
        port: Option<u16>,
        name: Option<String>,
        target_url: &str,
        mode: &str,
    ) -> Result<u16, ApiError> {
        let url = format!("{}/imposters", self.base_url);

        let request = CreateProxyImposterRequest {
            port,
            protocol: "http".to_string(),
            name,
            record_requests: true,
            stubs: vec![ProxyStub {
                responses: vec![ProxyResponse {
                    proxy: ProxyConfig {
                        to: target_url.to_string(),
                        mode: mode.to_string(),
                        predicate_generators: vec![PredicateGenerator {
                            matches: serde_json::json!({
                                "method": true,
                                "path": true,
                                "query": true
                            }),
                        }],
                        add_wait_behavior: true,
                    },
                }],
            }],
        };

        let resp = self.client.post(&url).json(&request).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        let detail: ImposterDetail = resp.json().await?;
        Ok(detail.port)
    }

    /// Clear proxy responses (saved recordings)
    pub async fn clear_proxy_responses(&self, port: u16) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/savedProxyResponses", self.base_url, port);
        let resp = self.client.delete(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        Ok(())
    }

    /// Export imposter config with recorded stubs (removeProxies=true)
    pub async fn export_imposter(
        &self,
        port: u16,
        remove_proxies: bool,
    ) -> Result<String, ApiError> {
        let url = if remove_proxies {
            format!(
                "{}/imposters/{}?replayable=true&removeProxies=true",
                self.base_url, port
            )
        } else {
            format!("{}/imposters/{}?replayable=true", self.base_url, port)
        };

        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        let json: serde_json::Value = resp.json().await?;
        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
    }

    /// Export all imposters as a single JSON document
    pub async fn export_all_imposters(&self) -> Result<String, ApiError> {
        let url = format!("{}/imposters?replayable=true", self.base_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return self.handle_error(resp).await;
        }

        let json: serde_json::Value = resp.json().await?;
        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
    }

    /// Replace all stubs for an imposter (used for reordering)
    pub async fn update_stubs(&self, port: u16, stubs: Vec<Stub>) -> Result<(), ApiError> {
        let url = format!("{}/imposters/{}/stubs", self.base_url, port);
        let body = serde_json::json!({ "stubs": stubs });
        let resp = self.client.put(&url).json(&body).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            self.handle_error(resp).await
        }
    }

    /// Get server configuration
    pub async fn get_config(&self) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/config", self.base_url);
        let resp = self.client.get(&url).send().await?;
        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            self.handle_error(resp).await
        }
    }

    /// Get metrics data
    pub async fn get_metrics(&self) -> Result<MetricsData, ApiError> {
        let url = format!("{}/metrics", self.base_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Ok(MetricsData::default());
        }

        let text = resp.text().await?;
        Ok(parse_prometheus_metrics(&text))
    }

    /// Handle error responses
    async fn handle_error<T>(&self, resp: reqwest::Response) -> Result<T, ApiError> {
        let status = resp.status();
        if let Ok(error_body) = resp.json::<ErrorResponse>().await
            && let Some(err) = error_body.errors.first()
        {
            return Err(ApiError::Server {
                code: err.code.clone(),
                message: err.message.clone(),
            });
        }
        Err(ApiError::Server {
            code: status.as_str().to_string(),
            message: format!("Request failed with status {status}"),
        })
    }
}

/// Parse Prometheus-format metrics into structured data
fn parse_prometheus_metrics(text: &str) -> MetricsData {
    let mut data = MetricsData::default();

    for line in text.lines() {
        // Skip comments and empty lines
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        // Parse rift_imposters_total
        if line.starts_with("rift_imposters_total")
            && let Some(value) = line.split_whitespace().last()
        {
            data.imposter_count = value.parse().unwrap_or(0);
        }

        // Parse rift_imposter_requests_total{port="..."} VALUE
        if line.starts_with("rift_imposter_requests_total")
            && let Some(port_start) = line.find("port=\"")
        {
            let port_str = &line[port_start + 6..];
            if let Some(port_end) = port_str.find('"')
                && let Ok(port) = port_str[..port_end].parse::<u16>()
                && let Some(value) = line.split_whitespace().last()
            {
                let count: u64 = value.parse().unwrap_or(0);
                data.total_requests += count;
                data.per_imposter.insert(
                    port,
                    ImposterMetrics {
                        request_count: count,
                        requests_per_second: 0.0,
                    },
                );
            }
        }
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_client_strips_trailing_slash() {
        let client = ApiClient::new("http://localhost:2525/");
        assert_eq!(client.base_url(), "http://localhost:2525");
    }

    #[test]
    fn test_api_client_no_trailing_slash() {
        let client = ApiClient::new("http://localhost:2525");
        assert_eq!(client.base_url(), "http://localhost:2525");
    }

    #[test]
    fn imposter_summary_parses_list_payload_fields() {
        // Issue #558 contract: the list response carries stubCount/enabled, so the list view
        // renders from one request — these fields must deserialize straight from the payload.
        let json = r#"{"imposters":[{"port":4545,"protocol":"http","numberOfRequests":7,"stubCount":3,"enabled":true}]}"#;
        let body: ImpostersResponse = serde_json::from_str(json).unwrap();
        let imp = &body.imposters[0];
        assert_eq!(imp.stub_count, 3);
        assert!(imp.enabled);
        assert_eq!(imp.number_of_requests, 7);
    }

    #[test]
    fn test_stub_deserialize_recorded_from() {
        let json = r#"{"predicates":[],"responses":[],"recordedFrom":"https://api.example.com"}"#;
        let stub: Stub = serde_json::from_str(json).unwrap();
        assert_eq!(
            stub.recorded_from.as_deref(),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn test_stub_deserialize_no_recorded_from() {
        let json = r#"{"predicates":[],"responses":[]}"#;
        let stub: Stub = serde_json::from_str(json).unwrap();
        assert!(stub.recorded_from.is_none());
    }

    #[test]
    fn test_stub_serialize_omits_none_fields() {
        let stub = Stub {
            scenario_name: None,
            id: None,
            recorded_from: None,
            predicates: vec![],
            responses: vec![],
        };
        let json = serde_json::to_string(&stub).unwrap();
        assert!(!json.contains("recordedFrom"));
        assert!(!json.contains("scenarioName"));
    }

    #[test]
    fn test_parse_prometheus_metrics() {
        let input = r#"
# HELP rift_imposters_total Total number of active imposters
# TYPE rift_imposters_total gauge
rift_imposters_total 3

# HELP rift_imposter_requests_total Total requests per imposter
# TYPE rift_imposter_requests_total counter
rift_imposter_requests_total{port="4545"} 42
rift_imposter_requests_total{port="4546"} 15
"#;

        let data = parse_prometheus_metrics(input);
        assert_eq!(data.imposter_count, 3);
        assert_eq!(data.total_requests, 57);
        assert_eq!(data.per_imposter.get(&4545).unwrap().request_count, 42);
        assert_eq!(data.per_imposter.get(&4546).unwrap().request_count, 15);
    }
}
