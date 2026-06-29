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
use crate::response::builder::ErrorResponseBuilder;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::{debug, error};

/// Helper function to create an error response.
pub fn error_response(status: u16, message: &str) -> Response<Full<Bytes>> {
    let body = format!(r#"{{"error": "{message}"}}"#);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from(r#"{"error": "internal"}"#))))
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
            ErrorResponseBuilder::new(StatusCode::BAD_GATEWAY)
                .header("content-type", "application/json")
                .body(r#"{"error": "Bad Gateway"}"#)
                .build_boxed()
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
            return ErrorResponseBuilder::new(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .body(r#"{"error": "Failed to read request body"}"#)
                .build_boxed();
        }
    };

    // Create request signature for recording lookup
    let signature =
        RequestSignature::new(method.as_str(), uri.path(), uri.query(), signature_headers);

    // Check if we should proxy or replay
    if !recording_store.should_proxy(&signature) {
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

    // Forward request and record response
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

    recording_store.record(signature, recorded_response.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
