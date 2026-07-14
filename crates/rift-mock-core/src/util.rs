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

/// Whether strict behavior mode is force-enabled process-wide, read once from the
/// `RIFT_STRICT_BEHAVIORS` env var (issue #375). When set (`1`/`true`/`yes`/`on`) it forces the
/// fail-loud contract on every imposter regardless of its per-imposter `strictBehaviors` flag — an
/// operational switch for CI runs that want a broken decorate/shellTransform/binary to surface as a
/// 500 rather than a header-annotated fallback body.
pub fn strict_behaviors_env() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        strict_behaviors_from(std::env::var("RIFT_STRICT_BEHAVIORS").ok().as_deref())
    })
}

/// Pure parse of the `RIFT_STRICT_BEHAVIORS` value, split out so it can be unit-tested without the
/// process-global env-var races that a full end-to-end test would hit.
fn strict_behaviors_from(val: Option<&str>) -> bool {
    val.map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Whether Rift is running in debug mode, read once from the `RIFT_DEBUG` env var (issue #359).
/// Response templating (`_rift.templated`) uses this to decide its error policy: a malformed or
/// failed `{{ }}` token is a request-time error in debug mode, or an empty-string substitution
/// plus a `tracing::warn!` otherwise. Follows the same on-values (`1`/`true`/`yes`/`on`) as
/// `RIFT_STRICT_BEHAVIORS`/`RIFT_DISABLE_HTTP2`.
pub fn rift_debug_env() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| rift_debug_from(std::env::var("RIFT_DEBUG").ok().as_deref()))
}

/// Pure parse of the `RIFT_DEBUG` value, split out so it can be unit-tested without the
/// process-global env-var races that a full end-to-end test would hit.
fn rift_debug_from(val: Option<&str>) -> bool {
    val.map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Percent-decode a query/form component, passing the raw text through unchanged if it is not
/// valid percent-encoding.
///
/// This is Rift's decode contract (issue #611): an undecodable sequence is the client's own text,
/// not an error to erase. Blanking it — the `unwrap_or_default()` this replaced — silently
/// destroys text a predicate matches on, and on a key it collapses distinct parameters into a
/// single `""` entry that then comma-joins unrelated values.
pub fn decode_or_raw(value: &str) -> String {
    urlencoding::decode(value)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

/// The terminal fallback for the builders below: a 500 assembled without the builder, so it cannot
/// itself fail. `Response::new` defaults to **200**, so the status must be set explicitly —
/// answering 200 with an error string is the failure-masking shape issue #611 sweeps out.
fn internal_error_fallback(e: &hyper::http::Error) -> Response<Full<Bytes>> {
    tracing::error!(error = %e, "response build failed; serving a 500");
    let mut resp = Response::new(Full::new(Bytes::from("Internal Server Error")));
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}

/// Build an HTTP response with the given status and body. Falls back to a minimal 500 if the
/// builder fails (which should not happen with a valid `StatusCode`).
pub fn build_response(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap_or_else(|e| internal_error_fallback(&e))
}

/// Build an HTTP response with headers. Falls back to a minimal 500 if the builder fails (e.g. a
/// caller-supplied header name/value that hyper rejects).
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
        .unwrap_or_else(|e| internal_error_fallback(&e))
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
    use super::{
        build_response, build_response_with_headers, http2_disabled_from, rift_debug_from,
        strict_behaviors_from,
    };
    use hyper::StatusCode;

    // Issue #611: the terminal builder fallback must not answer 200. A response-builder failure is
    // a server fault, and `Response::new` defaults to 200 — the same 200-masking shape #606 fixed
    // one helper up. An invalid header name is the only way to drive the builder's error path.
    #[test]
    fn build_response_with_headers_falls_back_to_500_on_builder_failure() {
        let resp = build_response_with_headers(StatusCode::OK, [("bad\nname", "v")], "x");
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a builder failure must surface as 500, not a 200 carrying an error string"
        );
    }

    // `build_response` takes a valid `StatusCode` and no headers, so its builder cannot fail today;
    // pin the success path so the fallback added for #611 can't silently change the happy status.
    #[test]
    fn build_response_preserves_the_requested_status() {
        assert_eq!(
            build_response(StatusCode::NOT_FOUND, "x").status(),
            StatusCode::NOT_FOUND
        );
    }

    // Issue #375: RIFT_STRICT_BEHAVIORS parsing — truthy values force strict, everything else lenient.
    #[test]
    fn strict_behaviors_env_parsing() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(
                strict_behaviors_from(Some(on)),
                "{on:?} should enable strict behaviors"
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
                !strict_behaviors_from(off),
                "{off:?} should keep behaviors lenient"
            );
        }
    }

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

    // Issue #359: RIFT_DEBUG parsing — truthy values enable request-time template errors.
    #[test]
    fn rift_debug_env_parsing() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(rift_debug_from(Some(on)), "{on:?} should enable debug mode");
        }
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("2"),
        ] {
            assert!(!rift_debug_from(off), "{off:?} should keep debug mode off");
        }
    }
}
