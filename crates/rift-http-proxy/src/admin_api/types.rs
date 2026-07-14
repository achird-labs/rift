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
    pub stub_count: usize,
    pub enabled: bool,
    pub record_requests: bool,
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

/// Pretty-print `body`, or give back the `500` to serve instead (issue #606).
///
/// A response body that cannot be rendered is a server fault, so the failure never inherits the
/// caller's intended status — a `201` that can't serialize is a `500`, not a `201` with the wrong
/// body. Callers that add headers should apply this first and return the error verbatim, so an
/// error response never carries metadata describing a payload that was never sent.
/// Boxed so the `Err` variant stays small (`clippy::result_large_err`); the allocation only ever
/// happens on the failure path.
pub(crate) fn serialize_or_500<T: Serialize>(
    body: &T,
) -> Result<String, Box<Response<Full<Bytes>>>> {
    serde_json::to_string_pretty(body).map_err(|e| {
        tracing::error!(error = %e, "failed to serialize admin API response body");
        Box::new(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to serialize response body: {e}"),
        ))
    })
}

/// Create a JSON response
pub fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    match serialize_or_500(body) {
        Ok(json) => {
            build_response_with_headers(status, [("Content-Type", "application/json")], json)
        }
        Err(resp) => *resp,
    }
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

/// Maximum admin-API request body accepted before responding `413 Payload Too
/// Large` (issue #546). The admin plane binds `0.0.0.0` and `--api-key` is
/// optional, so an unbounded body is a trivial memory-exhaustion vector. The
/// cap is deliberately generous — legitimate imposter/stub batches are well
/// under it — while bounding a single request's buffered size. Runtime
/// configurability was left out on purpose: it would mean threading a limit
/// through the whole handler dispatch for a value that never needs per-deploy
/// tuning; adjust this constant if that ever changes.
pub const MAX_ADMIN_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Error reading a request body into bytes. Its `Display` is surfaced as the
/// response body, so the wording is part of the admin API contract; the status
/// code comes from [`BodyError::status_code`].
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("Failed to read request body: {0}")]
    Read(String),
    #[error("Request body exceeds the {limit}-byte admin API limit")]
    TooLarge { limit: usize },
}

impl BodyError {
    /// HTTP status the admin API returns for this failure.
    pub fn status_code(&self) -> StatusCode {
        match self {
            BodyError::Read(_) => StatusCode::BAD_REQUEST,
            BodyError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }
}

/// Collect a request body into bytes, rejecting anything past
/// [`MAX_ADMIN_BODY_BYTES`] with [`BodyError::TooLarge`] instead of buffering
/// it all into memory (issue #546).
pub async fn collect_body(req: Request<Incoming>) -> Result<Bytes, BodyError> {
    collect_limited(req.into_body(), MAX_ADMIN_BODY_BYTES).await
}

/// Body-generic core of [`collect_body`], factored out so the size cap can be
/// unit-tested against a synthetic body without a live server.
async fn collect_limited<B>(body: B, limit: usize) -> Result<Bytes, BodyError>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use http_body_util::{BodyExt, Limited};
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(BodyError::TooLarge { limit })
        }
        Err(e) => Err(BodyError::Read(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// A payload whose `Serialize` always fails — the only way to drive serde's error path, since
    /// every real admin payload is plain structs/Vecs of strings and numbers (issue #606).
    struct Unserializable;
    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
    }

    async fn body_string(resp: Response<Full<Bytes>>) -> String {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    // AC1 (#606): a serialization failure is a server fault — 500 with the structured error
    // envelope, never 200 with a shape-invalid `{}` body that fails in the client's decoder.
    #[tokio::test]
    async fn json_response_maps_serialization_failure_to_500() {
        let resp = json_response(StatusCode::OK, &Unserializable);
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "the caller's OK is discarded: a body that cannot render is a server fault"
        );

        let body = body_string(resp).await;
        assert_ne!(
            body.trim(),
            "{}",
            "the silent empty-object body must be gone"
        );
        let json: serde_json::Value = serde_json::from_str(&body).expect("a structured envelope");
        assert!(
            json["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("serialize")),
            "the envelope names the real cause, not the client's codec: {body}"
        );
    }

    // AC2 (#606): the success path is untouched — status, content type, and the exact
    // pretty-printed bytes SDK codecs and Mountebank compat already depend on.
    #[tokio::test]
    async fn json_response_success_path_unchanged() {
        let payload = serde_json::json!({"a": 1, "b": ["x"]});
        let resp = json_response(StatusCode::CREATED, &payload);
        assert_eq!(resp.status(), StatusCode::CREATED, "given status preserved");
        assert_eq!(
            resp.headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            body_string(resp).await,
            serde_json::to_string_pretty(&payload).expect("pretty"),
            "body stays byte-identical to the pretty-printed payload"
        );
    }

    // The shared helper both `json_response` and `cursor_response` route through, so the two
    // paths of the savedRequests handler cannot drift apart again. The header-ordering half of
    // AC3 lives with `build_cursor_response` in handlers/imposters.rs — asserting it here would
    // only restate that `error_response` has no cursor headers, which is true regardless of what
    // the cursor path does.
    #[tokio::test]
    async fn serialize_or_500_yields_json_or_a_500_response() {
        let ok = serialize_or_500(&serde_json::json!({"a": 1}));
        assert_eq!(
            ok.ok().as_deref(),
            Some(
                serde_json::to_string_pretty(&serde_json::json!({"a": 1}))
                    .unwrap()
                    .as_str()
            )
        );

        let err = *serialize_or_500(&Unserializable).expect_err("must not serialize");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn collect_body_under_limit_returns_bytes() {
        let payload = Bytes::from(vec![b'a'; 1024]);
        let body = Full::new(payload.clone());
        let got = collect_limited(body, 4096).await.expect("under limit");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn collect_body_at_limit_is_accepted() {
        let payload = Bytes::from(vec![b'a'; 4096]);
        let body = Full::new(payload.clone());
        let got = collect_limited(body, 4096).await.expect("exactly at limit");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn collect_body_over_limit_returns_too_large() {
        let body = Full::new(Bytes::from(vec![b'a'; 4097]));
        let err = collect_limited(body, 4096).await.expect_err("over limit");
        assert!(matches!(err, BodyError::TooLarge { limit: 4096 }));
    }

    #[test]
    fn body_error_status_codes() {
        assert_eq!(
            BodyError::Read("boom".to_string()).status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            BodyError::TooLarge { limit: 64 }.status_code(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

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
