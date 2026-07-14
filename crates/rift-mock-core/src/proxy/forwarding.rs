//! Request forwarding logic for the proxy server.
//!
//! This module handles forwarding requests to upstream servers,
//! including support for recording (Mountebank-compatible).

use super::client::HttpClient;
use super::headers::{
    RiftHeadersExt, VALUE_TRUE, X_RIFT_PROXIED, X_RIFT_RECORDED, X_RIFT_REPLAYED,
};
use super::response_ext::ResponseExt;
use crate::recording::{ProxyMode, RecordedResponse, RecordingStore, RequestSignature};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::{debug, error};

/// Helper function to create an error response.
///
/// Delegates to the crate's canonical Mountebank-shaped error builder (issue #611) rather than
/// interpolating the message into a JSON string literal, which produced invalid JSON whenever the
/// message contained a quote. A `status` hyper cannot represent is a server fault, not something
/// to serve as-is, so it becomes a loud 500.
pub fn error_response(status: u16, message: &str) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(status).unwrap_or_else(|e| {
        error!(status, error = %e, "invalid HTTP status for error response; serving 500");
        StatusCode::INTERNAL_SERVER_ERROR
    });
    crate::response::error_response(status, message)
}

/// Build a `Request::Builder` pointing at the upstream, with headers copied
/// (skipping `host`). Callers attach the body and send.
fn build_upstream_request(
    method: hyper::Method,
    uri: &hyper::Uri,
    headers: &hyper::HeaderMap,
    upstream_uri: &str,
) -> hyper::http::request::Builder {
    let upstream_path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let full_uri = format!("{upstream_uri}{upstream_path}");

    let mut builder = Request::builder().method(method).uri(full_uri);

    for (key, value) in headers.iter() {
        if key != "host" {
            builder = builder.header(key, value);
        }
    }

    builder
}

/// Forward a request with a pre-collected body.
pub async fn forward_request_with_body(
    http_client: &HttpClient,
    method: hyper::Method,
    uri: hyper::Uri,
    headers: hyper::HeaderMap,
    body_bytes: Bytes,
    upstream_uri: &str,
) -> Response<Full<Bytes>> {
    let builder = build_upstream_request(method, &uri, &headers, upstream_uri);

    debug!(
        "Forwarding to: {}",
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    let upstream_req = builder
        .body(BoxBody::new(
            Full::new(body_bytes).map_err(|never: Infallible| match never {}),
        ))
        .unwrap();

    match http_client.request(upstream_req).await {
        Ok(upstream_response) => {
            let (parts, body) = upstream_response.into_parts();
            let body_bytes = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(e) => {
                    error!("Failed to collect upstream response body: {}", e);
                    return error_response(502, "Failed to read upstream response");
                }
            };
            let mut response = Response::from_parts(parts, Full::new(body_bytes));
            response.set_header(&X_RIFT_PROXIED, &VALUE_TRUE);
            response
        }
        Err(e) => {
            error!("Failed to forward request to upstream: {}", e);
            error_response(502, "Bad Gateway")
        }
    }
}

/// Forward a request with streaming body (no buffering).
pub async fn forward_request_streaming(
    http_client: &HttpClient,
    req: Request<hyper::body::Incoming>,
    upstream_uri: &str,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    let builder = build_upstream_request(method, &uri, &headers, upstream_uri);

    debug!(
        "Forwarding (streaming) to: {}",
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    // Pass request body through directly without buffering
    let upstream_req = builder.body(BoxBody::new(req.into_body())).unwrap();

    // Forward with streaming response
    match http_client.request(upstream_req).await {
        Ok(upstream_response) => {
            let (mut parts, body) = upstream_response.into_parts();
            parts.set_header(&X_RIFT_PROXIED, &VALUE_TRUE);
            Response::from_parts(parts, BoxBody::new(body))
        }
        Err(e) => {
            error!("Failed to forward request to upstream: {}", e);
            // Same helper as the buffered path (issue #611) so an upstream failure produces one
            // error envelope regardless of which proxy mode happens to be serving.
            error_response(502, "Bad Gateway").into_boxed()
        }
    }
}

/// Forward request with recording support (Mountebank-compatible proxyOnce/proxyAlways).
pub async fn forward_with_recording(
    http_client: &HttpClient,
    recording_store: &Arc<RecordingStore>,
    signature_headers: &[(String, String)],
    req: Request<hyper::body::Incoming>,
    upstream_uri: &str,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // For recording modes, we need to collect the body to create a signature
    let mode = recording_store.mode();
    if mode == ProxyMode::ProxyTransparent {
        // Transparent mode - no recording, use streaming
        return forward_request_streaming(http_client, req, upstream_uri).await;
    }

    // Collect body for signature creation
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to collect request body for recording: {}", e);
            return error_response(500, "Failed to read request body").into_boxed();
        }
    };

    // Create request signature for recording lookup
    let signature =
        RequestSignature::new(method.as_str(), uri.path(), uri.query(), signature_headers);

    // Check if we should proxy or replay. `should_proxy` returning true means we took the
    // proxyOnce pending claim; guard it so a forward cancelled before record() (client disconnect)
    // releases the claim instead of wedging the signature as permanently pending (issue #555).
    let claimed = recording_store.should_proxy(&signature);
    if !claimed {
        // Return recorded response (proxyOnce mode with existing recording)
        if let Some(recorded) = recording_store.get_recorded(&signature) {
            debug!(
                "Replaying recorded response for {} {} (status: {})",
                method,
                uri.path(),
                recorded.status
            );

            let mut response = Response::builder().status(recorded.status);

            // Restore recorded headers
            for (key, value) in &recorded.headers {
                if let Ok(header_value) = value.parse::<hyper::header::HeaderValue>() {
                    response = response.header(key.as_str(), header_value);
                }
            }

            // Add replay indicator header
            response = response.header(X_RIFT_REPLAYED.clone(), VALUE_TRUE.clone());

            return response
                .body(BoxBody::new(
                    Full::new(Bytes::from(recorded.body.clone()))
                        .map_err(|never: Infallible| match never {}),
                ))
                .unwrap();
        }
    }

    // Forward request and record response. Only proxyOnce takes a pending claim (other modes'
    // `should_proxy` returns true without claiming), so arm the guard only there; it releases the
    // claim if this future is dropped (cancelled) before record() runs below.
    let claim_guard = (claimed && recording_store.mode() == ProxyMode::ProxyOnce)
        .then(|| PendingClaimGuard::new(recording_store, signature.clone()));

    let start = std::time::Instant::now();
    let response = forward_request_with_body(
        http_client,
        method.clone(),
        uri.clone(),
        headers,
        body_bytes,
        upstream_uri,
    )
    .await;

    let latency_ms = start.elapsed().as_millis() as u64;

    // Record the response
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();

    // Extract body bytes for recording
    let response_body_bytes: Bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };

    // Extract headers for recording (preserving multi-valued headers)
    let mut recorded_headers = Vec::new();
    for (key, value) in parts.headers.iter() {
        if let Ok(value_str) = value.to_str() {
            recorded_headers.push((key.as_str().to_string(), value_str.to_string()));
        }
    }

    // Record the response
    let recorded_response = RecordedResponse {
        status,
        headers: recorded_headers,
        body: response_body_bytes.to_vec(),
        latency_ms: Some(latency_ms),
        timestamp_secs: crate::util::unix_timestamp(),
    };

    recording_store.record(signature, recorded_response);
    // record() cleared the claim; disarm so the guard's drop doesn't touch a signature a
    // subsequent proxyOnce cycle may have re-claimed.
    if let Some(guard) = claim_guard {
        guard.disarm();
    }
    debug!(
        "Recorded response for {} {} (status: {}, latency: {}ms)",
        method,
        uri.path(),
        status,
        latency_ms
    );

    // Reconstruct response
    let mut response = Response::from_parts(parts, Full::new(response_body_bytes));
    response.set_header(&X_RIFT_RECORDED, &VALUE_TRUE);

    response.into_boxed()
}

/// Releases a `proxyOnce` pending claim on drop unless disarmed. `should_proxy` takes the claim
/// but only `record()` clears it, so a forward that is cancelled (client disconnect drops the
/// task) between the two would wedge the signature as permanently pending. Arming this guard for
/// the duration of the forward, then disarming it once `record()` has cleared the claim, releases
/// the claim on the cancel/early-return path without double-releasing on the happy path (issue
/// #555). A pending signature is exclusively held, so releasing our own abandoned claim never
/// touches another request's.
struct PendingClaimGuard<'a> {
    store: &'a RecordingStore,
    signature: RequestSignature,
    armed: bool,
}

impl<'a> PendingClaimGuard<'a> {
    fn new(store: &'a RecordingStore, signature: RequestSignature) -> Self {
        Self {
            store,
            signature,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingClaimGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.store.release_pending(&self.signature);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // Issue #555: the claim guard releases a pending proxyOnce claim when the forward is dropped
    // (task cancelled) before record(), but a guard disarmed after a successful record() must NOT
    // touch the pending set (record() already cleared it).
    #[test]
    fn claim_guard_releases_on_drop_unless_disarmed() {
        let store = Arc::new(RecordingStore::new(ProxyMode::ProxyOnce));
        let sig = RequestSignature::new("GET", "/guarded", None, &[]);

        // Armed guard dropped without disarm → claim released, proxying re-opens.
        assert!(store.should_proxy(&sig), "claim taken");
        {
            let _guard = PendingClaimGuard::new(&store, sig.clone());
        }
        assert!(
            store.should_proxy(&sig),
            "dropping an armed guard releases the abandoned claim"
        );

        // Disarmed guard drop is a no-op: it must not release a claim record() would own.
        {
            let guard = PendingClaimGuard::new(&store, sig.clone());
            guard.disarm();
        }
        assert!(
            !store.should_proxy(&sig),
            "a disarmed guard must not release the still-pending claim"
        );
    }

    #[test]
    fn test_error_response_basic() {
        let response = error_response(500, "Internal Server Error");
        assert_eq!(response.status(), 500);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_error_response_400() {
        let response = error_response(400, "Bad Request");
        assert_eq!(response.status(), 400);
    }

    #[test]
    fn test_error_response_502() {
        let response = error_response(502, "Bad Gateway");
        assert_eq!(response.status(), 502);
    }

    #[test]
    fn test_error_response_404() {
        let response = error_response(404, "Not Found");
        assert_eq!(response.status(), 404);
    }

    #[test]
    fn test_error_response_503() {
        let response = error_response(503, "Service Unavailable");
        assert_eq!(response.status(), 503);
    }

    async fn body_string(response: Response<Full<Bytes>>) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    // Issue #611: the body was built by string interpolation, so a message containing a quote
    // produced invalid JSON that only fails in the client's decoder. It must always parse.
    #[tokio::test]
    async fn error_response_escapes_quotes_in_the_message() {
        let response = error_response(502, r#"upstream said "no""#);
        let body = body_string(response).await;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("error body must be valid JSON");
        assert_eq!(
            parsed["errors"][0]["message"],
            serde_json::json!(r#"upstream said "no""#),
            "the message must survive escaping intact"
        );
    }

    // Issue #611: `status: u16` let a code the builder rejects fall into the terminal fallback,
    // which answered 200. An unrepresentable code is a server fault → 500, never a masked 200.
    #[tokio::test]
    async fn error_response_maps_an_invalid_status_code_to_500() {
        for bad in [0u16, 1000] {
            let response = error_response(bad, "boom");
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "status {bad} is not a valid HTTP status; must fall back to 500"
            );
        }
    }

    // Pins the Mountebank error envelope the proxy now shares with the rest of the crate.
    #[tokio::test]
    async fn error_response_body_is_the_mountebank_error_envelope() {
        let response = error_response(502, "Bad Gateway");
        let body = body_string(response).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["errors"][0]["code"], serde_json::json!("502"));
        assert_eq!(
            parsed["errors"][0]["message"],
            serde_json::json!("Bad Gateway")
        );
    }
}
