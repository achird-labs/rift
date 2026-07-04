//! Shared utility functions used across proxy recording, stub generation,
//! and response building pipelines.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether HTTP/2 auto-negotiation should be force-disabled on the serve listeners, read once from
/// the `RIFT_DISABLE_HTTP2` env var (issue #378). An operational escape hatch: the listeners
/// auto-negotiate HTTP/1 and HTTP/2 by default (#295); set this (`1`/`true`/`yes`/`on`) to serve
/// HTTP/1 only if a client misbehaves over HTTP/2.
pub fn http2_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED
        .get_or_init(|| http2_disabled_from(std::env::var("RIFT_DISABLE_HTTP2").ok().as_deref()))
}

/// Pure parse of the `RIFT_DISABLE_HTTP2` value, split out so it can be unit-tested without the
/// process-global env-var races that a full end-to-end test would hit.
fn http2_disabled_from(val: Option<&str>) -> bool {
    val.map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Build an HTTP response with the given status and body. Falls back to a minimal 500 if the
/// builder fails (which should not happen with a valid `StatusCode`).
pub fn build_response(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("Internal Server Error"))))
}

/// Build an HTTP response with headers. Falls back to a minimal 500 if the builder fails.
pub fn build_response_with_headers(
    status: StatusCode,
    headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    for (key, value) in headers {
        builder = builder.header(key.as_ref(), value.as_ref());
    }
    builder
        .body(Full::new(body.into()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("Internal Server Error"))))
}

/// Merge a slice of `(key, value)` header pairs into a `HashMap`,
/// comma-joining values for duplicate keys per HTTP spec (RFC 9110 §5.3).
pub fn merge_headers_to_map(headers: &[(String, String)]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (k, v) in headers {
        map.entry(k.clone())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(v);
            })
            .or_insert_with(|| v.clone());
    }
    map
}

/// Returns `true` for hop-by-hop headers that should be stripped when
/// building stubs or forwarding proxy responses.
pub fn is_hop_by_hop_header(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "transfer-encoding" || lower == "connection" || lower == "keep-alive"
}

/// Encode a body for storage in a stub.
///
/// - If `body` is empty, returns `(None, false)`.
/// - If `body` is valid UTF-8, tries to parse as JSON; falls back to a plain string.
///   Returns `(Some(value), false)`.
/// - If `body` is not valid UTF-8, base64-encodes it and returns `(Some(encoded), true)`.
pub fn encode_body_for_stub(body: &[u8]) -> (Option<serde_json::Value>, bool) {
    if body.is_empty() {
        return (None, false);
    }

    match std::str::from_utf8(body) {
        Ok(text) => {
            let val = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(text) {
                json_val
            } else {
                serde_json::Value::String(text.to_string())
            };
            (Some(val), false)
        }
        Err(_) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(body);
            (Some(serde_json::Value::String(encoded)), true)
        }
    }
}

/// Get current unix timestamp in seconds.
pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::http2_disabled_from;

    // Issue #378: RIFT_DISABLE_HTTP2 parsing — truthy values disable, everything else keeps HTTP/2.
    #[test]
    fn http2_disable_env_parsing() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(
                http2_disabled_from(Some(on)),
                "{on:?} should disable HTTP/2"
            );
        }
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("2"),
        ] {
            assert!(
                !http2_disabled_from(off),
                "{off:?} should keep HTTP/2 enabled"
            );
        }
    }
}
