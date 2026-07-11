//! Response types and HATEOAS structures for the Admin API.

use crate::extensions::stub_analysis::StubWarning;
use crate::imposter::{RecordedRequest, Stub};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

/// HATEOAS link structure for Mountebank compatibility
#[derive(Debug, Serialize, Clone)]
pub struct Link {
    pub href: String,
}

/// HATEOAS links for imposter resources
#[derive(Debug, Serialize, Clone)]
pub struct ImposterLinks {
    #[serde(rename = "self")]
    pub self_link: Link,
    pub stubs: Link,
}

/// HATEOAS links for stub resources
#[derive(Debug, Serialize, Clone)]
pub struct StubLinks {
    #[serde(rename = "self")]
    pub self_link: Link,
}

/// Imposter summary for list responses
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposterSummary {
    pub protocol: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub number_of_requests: u64,
    pub enabled: bool,
    #[serde(rename = "_links")]
    pub links: ImposterLinks,
}

/// Response for listing imposters
#[derive(Debug, Serialize)]
pub struct ListImpostersResponse {
    pub imposters: Vec<ImposterSummary>,
}

/// A stub with its _links for the response
#[derive(Debug, Serialize)]
pub struct StubWithLinks {
    #[serde(flatten)]
    pub stub: Stub,
    #[serde(rename = "_links")]
    pub links: StubLinks,
}

/// Detailed imposter response
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposterDetail {
    pub protocol: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub number_of_requests: u64,
    pub enabled: bool,
    pub record_requests: bool,
    pub requests: Vec<RecordedRequest>,
    pub stubs: Vec<StubWithLinks>,
    #[serde(rename = "_links")]
    pub links: ImposterLinks,
    /// Rift extensions - includes stub analysis warnings
    #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
    pub rift: Option<RiftImposterExtensions>,
}

/// Rift-specific extensions in API responses
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RiftImposterExtensions {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StubWarning>,
    /// The imposter's flow-state config (issue #260), so tools like `rift-verify` can learn the
    /// correlated-isolation `flowIdSource` header. Redacted: the `redis` block (which may carry a
    /// credentialed connection URL) is stripped before exposure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_state: Option<serde_json::Value>,
}

// Error types + `error_response` + the `From<ImposterError>` conversion moved to
// `rift_mock_core::response` (issue #203). Re-exported so admin call sites are unchanged.
pub use rift_mock_core::response::error_response;

/// Request to add a stub
#[derive(Debug, Deserialize)]
pub struct AddStubRequest {
    #[serde(default)]
    pub index: Option<usize>,
    pub stub: Stub,
}

/// Request to replace all stubs
#[derive(Debug, Deserialize)]
pub struct ReplaceStubsRequest {
    pub stubs: Vec<Stub>,
}

/// Query parameters for imposter endpoints
#[derive(Debug, Default)]
pub struct ImposterQueryParams {
    pub replayable: bool,
    pub remove_proxies: bool,
    pub list: bool,
}

impl ImposterQueryParams {
    /// Parse query parameters from query string
    pub fn parse(query: Option<&str>) -> Self {
        let mut params = Self::default();
        if let Some(q) = query {
            params.replayable = q.contains("replayable=true");
            params.remove_proxies = q.contains("removeProxies=true");
            params.list = q.contains("list=true");
        }
        params
    }
}

/// Minimal imposter listing entry (Mountebank ?list=true response shape)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposterListEntry {
    pub protocol: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub number_of_requests: u64,
    #[serde(rename = "_links")]
    pub links: ImposterLinks,
}

// =============================================================================
// Helper functions for generating HATEOAS links
// =============================================================================

/// Extract base URL from request headers for HATEOAS links.
pub fn get_base_url(req: &Request<Incoming>) -> String {
    base_url_from_headers(req.headers())
}

/// Inner helper so the sanitization logic is unit-testable without a live `Request<Incoming>`.
///
/// Rejects Host header values containing `/` or `://` to prevent link-injection via a
/// malformed Host header (e.g. `attacker.com/evil`).
fn base_url_from_headers(headers: &hyper::HeaderMap) -> String {
    if let Some(host) = headers.get("host")
        && let Ok(host_str) = host.to_str()
        && !host_str.contains('/')
        && !host_str.contains("://")
    {
        return format!("http://{host_str}");
    }
    format!("http://localhost:{}", super::DEFAULT_ADMIN_PORT)
}

/// Generate HATEOAS links for an imposter
pub fn make_imposter_links(base_url: &str, port: u16) -> ImposterLinks {
    ImposterLinks {
        self_link: Link {
            href: format!("{base_url}/imposters/{port}"),
        },
        stubs: Link {
            href: format!("{base_url}/imposters/{port}/stubs"),
        },
    }
}

/// Generate HATEOAS links for a stub
pub fn make_stub_links(base_url: &str, port: u16, index: usize) -> StubLinks {
    StubLinks {
        self_link: Link {
            href: format!("{base_url}/imposters/{port}/stubs/{index}"),
        },
    }
}

// =============================================================================
// Response helper functions
// =============================================================================

/// Create a JSON response
pub fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let json = serde_json::to_string_pretty(body).unwrap_or_else(|_| "{}".to_string());
    build_response_with_headers(status, [("Content-Type", "application/json")], json)
}

// The generic response builders moved to `rift_mock_core::util` (issue #203) so the engine can use
// them without depending on the admin server. Re-exported here so `crate::admin_api::types::*`
// call sites are unchanged.
pub use rift_mock_core::util::build_response_with_headers;

/// Create a not found response
pub fn not_found() -> Response<Full<Bytes>> {
    error_response(StatusCode::NOT_FOUND, "Not Found")
}

/// Create an "imposter not found" response for the given port
pub fn imposter_not_found(port: u16) -> Response<Full<Bytes>> {
    error_response(
        StatusCode::NOT_FOUND,
        &format!("Imposter not found on port {port}"),
    )
}

/// Error reading a request body into bytes. Its `Display` is surfaced as the
/// `400 Bad Request` body, so the wording is part of the admin API contract.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("Failed to read request body: {0}")]
    Read(#[from] hyper::Error),
}

/// Collect request body into bytes
pub async fn collect_body(req: Request<Incoming>) -> Result<Bytes, BodyError> {
    use http_body_util::BodyExt;
    Ok(req.collect().await?.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_imposter_query_params_parse() {
        let params = ImposterQueryParams::parse(Some("replayable=true&removeProxies=true"));
        assert!(params.replayable);
        assert!(params.remove_proxies);
        assert!(!params.list);

        let params = ImposterQueryParams::parse(Some("replayable=false"));
        assert!(!params.replayable);
        assert!(!params.remove_proxies);
        assert!(!params.list);

        let params = ImposterQueryParams::parse(None);
        assert!(!params.replayable);
        assert!(!params.remove_proxies);
        assert!(!params.list);

        let params = ImposterQueryParams::parse(Some("list=true"));
        assert!(!params.replayable);
        assert!(!params.remove_proxies);
        assert!(params.list);

        let params = ImposterQueryParams::parse(Some("list=true&replayable=true"));
        assert!(params.replayable);
        assert!(params.list);
    }

    #[test]
    fn test_imposter_list_entry_excludes_enabled() {
        let entry = ImposterListEntry {
            protocol: "http".to_string(),
            port: 8080,
            name: None,
            number_of_requests: 0,
            links: make_imposter_links("http://localhost:2525", 8080),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(
            json.get("enabled").is_none(),
            "list entry must not include 'enabled'"
        );
        assert!(json.get("numberOfRequests").is_some());
        assert!(json.get("_links").is_some());
    }

    #[test]
    fn test_make_imposter_links() {
        let links = make_imposter_links("http://localhost:2525", 8080);
        assert_eq!(links.self_link.href, "http://localhost:2525/imposters/8080");
        assert_eq!(
            links.stubs.href,
            "http://localhost:2525/imposters/8080/stubs"
        );
    }

    #[test]
    fn test_make_stub_links() {
        let links = make_stub_links("http://localhost:2525", 8080, 0);
        assert_eq!(
            links.self_link.href,
            "http://localhost:2525/imposters/8080/stubs/0"
        );
    }

    #[test]
    fn test_error_response_format() {
        let resp = error_response(StatusCode::BAD_REQUEST, "Test error");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_json_response() {
        let body = serde_json::json!({"test": "value"});
        let resp = json_response(StatusCode::OK, &body);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_not_found_response() {
        let resp = not_found();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_base_url_from_valid_host() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("host", "example.com:2525".parse().unwrap());
        assert_eq!(base_url_from_headers(&headers), "http://example.com:2525");
    }

    #[test]
    fn test_base_url_from_host_no_port() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("host", "localhost".parse().unwrap());
        assert_eq!(base_url_from_headers(&headers), "http://localhost");
    }

    #[test]
    fn test_base_url_rejects_host_with_path() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("host", "attacker.com/evil".parse().unwrap());
        // hyper's HeaderValue parser will accept this string, but our check rejects it
        assert_eq!(
            base_url_from_headers(&headers),
            "http://localhost:2525",
            "Host with path segment must fall back to default"
        );
    }

    #[test]
    fn test_base_url_rejects_host_with_scheme() {
        // hyper rejects "http://..." in the Host header at the header-value level, so we test
        // our guard with a value that contains "://" but slips through as a raw string.
        let mut headers = hyper::HeaderMap::new();
        // Use a raw insert via from_bytes to bypass high-level validation
        if let Ok(v) = hyper::header::HeaderValue::from_bytes(b"http://attacker.com") {
            headers.insert("host", v);
            assert_eq!(
                base_url_from_headers(&headers),
                "http://localhost:2525",
                "Host with scheme must fall back to default"
            );
        }
    }

    #[test]
    fn test_base_url_no_host_header() {
        let headers = hyper::HeaderMap::new();
        assert_eq!(base_url_from_headers(&headers), "http://localhost:2525");
    }
}
