//! The status a response is served with, as Mountebank decides it (issue #1558).
//!
//! Mountebank keeps `statusCode` as written, substitutes `copy`/`lookup` tokens into it, and reads
//! it only when it writes the response: `postProcess` falls back past a falsy one
//! (`stubResponse.statusCode || defaultResponse.statusCode || 200`, `baseHttpServer.js:25`), then
//! Node's `writeHead` reads it as `Number(value) | 0` and throws `ERR_HTTP_INVALID_STATUS_CODE`
//! outside 100–999, which Mountebank answers with a `500` (`:143-155`).

use super::types::IsResponse;
use crate::util::build_response_with_headers;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};

/// The status to serve for `code`, or for `text` when the `statusCode` is a string (behaviors
/// already applied). A falsy value — `""` or `0` — takes `default`'s, then `200`. `Err` holds the
/// value Node refuses, as Mountebank's error message quotes it.
pub(crate) fn resolve(
    code: u16,
    text: Option<&str>,
    default: Option<&IsResponse>,
) -> Result<StatusCode, String> {
    if truthy(code, text) {
        return read(code, text);
    }
    if let Some(default) = default.filter(|d| !d.omitted.status_code) {
        let default_text = default.status_text.as_deref();
        if truthy(default.status_code, default_text) {
            return read(default.status_code, default_text);
        }
    }
    Ok(StatusCode::OK)
}

/// JavaScript truthiness of a `statusCode`.
fn truthy(code: u16, text: Option<&str>) -> bool {
    match text {
        Some(text) => !text.is_empty(),
        None => code != 0,
    }
}

/// Node's `writeHead` reading of a truthy status.
fn read(code: u16, text: Option<&str>) -> Result<StatusCode, String> {
    let served = match text {
        Some(text) => rift_types::status::node_status_code(text).ok_or_else(|| text.to_string())?,
        None if (100..=999).contains(&code) => code,
        None => return Err(code.to_string()),
    };
    // `hyper` takes exactly 100–999, the range just checked.
    StatusCode::from_u16(served).map_err(|_| served.to_string())
}

/// Mountebank's answer to a status Node refuses: `500` with the `RangeError` as its error envelope.
/// Mountebank's also carries the error's `stack`, which names Node internals; it is left out.
pub(crate) fn invalid_status_response(value: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "errors": [{
            "code": "ERR_HTTP_INVALID_STATUS_CODE",
            "message": format!("Invalid status code: {value}"),
            "name": "RangeError",
        }]
    })
    .to_string();
    build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [
            ("x-rift-imposter", "true"),
            ("content-type", "application/json"),
        ],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_with(status: serde_json::Value) -> IsResponse {
        serde_json::from_value(serde_json::json!({ "statusCode": status })).expect("decodes")
    }

    #[test]
    fn a_code_in_range_is_served_and_one_outside_is_refused() {
        assert_eq!(resolve(404, None, None), Ok(StatusCode::NOT_FOUND));
        assert_eq!(resolve(999, None, None).map(|s| s.as_u16()), Ok(999));
        assert_eq!(resolve(99, None, None), Err("99".to_string()));
        assert_eq!(resolve(1000, None, None), Err("1000".to_string()));
    }

    #[test]
    fn a_string_is_read_as_node_reads_it() {
        assert_eq!(resolve(200, Some(" 404 "), None), Ok(StatusCode::NOT_FOUND));
        assert_eq!(resolve(200, Some("0x194"), None), Ok(StatusCode::NOT_FOUND));
        assert_eq!(
            resolve(200, Some("${code}"), None),
            Err("${code}".to_string())
        );
    }

    #[test]
    fn a_falsy_status_takes_the_default_then_200() {
        let teapot = default_with(serde_json::json!(418));
        assert_eq!(resolve(200, Some(""), None), Ok(StatusCode::OK));
        assert_eq!(
            resolve(200, Some(""), Some(&teapot)).map(|s| s.as_u16()),
            Ok(418)
        );
        assert_eq!(resolve(0, None, Some(&teapot)).map(|s| s.as_u16()), Ok(418));
        // A default that is itself falsy, or omits its status, gives 200.
        let empty = default_with(serde_json::json!(""));
        assert_eq!(resolve(200, Some(""), Some(&empty)), Ok(StatusCode::OK));
        let omitted: IsResponse = serde_json::from_value(serde_json::json!({})).expect("decodes");
        assert_eq!(resolve(0, None, Some(&omitted)), Ok(StatusCode::OK));
        // A default token is read as written, and refused.
        let token = default_with(serde_json::json!("${x}"));
        assert_eq!(
            resolve(200, Some(""), Some(&token)),
            Err("${x}".to_string())
        );
    }
}
