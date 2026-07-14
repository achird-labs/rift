//! Request context for behavior processing.

use std::collections::HashMap;

/// Convert a header name to title case (e.g., "content-type" -> "Content-Type").
///
/// This is used for Mountebank compatibility, which expects title-cased header names.
pub fn header_to_title_case(name: &str) -> String {
    let mut title_case = String::with_capacity(name.len());
    for part in name.split_inclusive('-') {
        let mut chars = part.chars();
        if let Some(first_char) = chars.next() {
            title_case.push(first_char.to_ascii_uppercase());
        }
        title_case.push_str(chars.as_str());
    }
    title_case
}

/// Request context for behavior processing
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
}

impl RequestContext {
    /// Create from hyper request parts
    pub fn from_request(
        method: &str,
        uri: &hyper::Uri,
        headers: &hyper::HeaderMap,
        body: Option<&str>,
    ) -> Self {
        let mut query_map = HashMap::new();
        if let Some(query) = uri.query() {
            for pair in query.split('&').filter(|s| !s.is_empty()) {
                let (key, value) = match pair.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (pair, ""),
                };
                let decoded_key = key.to_string();
                let decoded_value = crate::util::decode_or_raw(value);
                query_map
                    .entry(decoded_key)
                    .and_modify(|existing: &mut String| {
                        existing.push(',');
                        existing.push_str(&decoded_value);
                    })
                    .or_insert(decoded_value);
            }
        }

        let mut header_map = HashMap::new();
        for (name, value) in headers.iter() {
            // Coerce a non-UTF-8 value to "" rather than dropping the header (issue #480): the
            // request hot path now passes hyper's raw HeaderMap, and a dropped key would flip a
            // header from present-but-empty to absent for behaviors/predicates that read it.
            let v = value.to_str().unwrap_or("");
            header_map.insert(header_to_title_case(name.as_str()), v.to_string());
        }

        Self {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: query_map,
            headers: header_map,
            body: body.map(|s| s.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #611: an undecodable percent-sequence used to blank the value (`unwrap_or_default`),
    // silently destroying text a predicate matches on. Every sibling decode site in the repo
    // (request_filter.rs, intercept.rs, scenarios.rs) passes the raw value through instead.
    #[test]
    fn from_request_passes_through_an_undecodable_query_value() {
        let uri: hyper::Uri = "/p?k=%FF".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &hyper::HeaderMap::new(), None);
        assert_eq!(
            ctx.query.get("k").map(String::as_str),
            Some("%FF"),
            "an undecodable value must pass through raw, not become an empty string"
        );
    }

    #[test]
    fn from_request_still_decodes_a_valid_query_value() {
        let uri: hyper::Uri = "/p?k=hello%20world".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &hyper::HeaderMap::new(), None);
        assert_eq!(ctx.query.get("k").map(String::as_str), Some("hello world"));
    }

    // Issue #480 — the request context is now built from `req.headers().clone()`, whose names are
    // hyper's lowercase form, instead of a HashMap that was pre-title-cased. `from_request` must
    // title-case the names itself so the resulting context is unchanged regardless of input casing.
    #[test]
    fn from_request_title_cases_lowercase_headermap() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::HeaderName::from_static("content-type"),
            hyper::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            hyper::header::HeaderName::from_static("x-custom-header"),
            hyper::header::HeaderValue::from_static("v"),
        );
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert_eq!(
            ctx.headers.get("Content-Type").map(String::as_str),
            Some("application/json"),
            "header names must be Title-Case regardless of the input HeaderMap's casing"
        );
        assert_eq!(
            ctx.headers.get("X-Custom-Header").map(String::as_str),
            Some("v")
        );
    }

    // Issue #480 — the hot path now passes hyper's raw HeaderMap, which can hold a value that is not
    // valid UTF-8. Such a header must stay PRESENT (coerced to "") rather than being silently
    // dropped, preserving the prior request-context behavior for behaviors/predicates that read it.
    #[test]
    fn from_request_keeps_non_utf8_header_as_empty() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::HeaderName::from_static("x-bin"),
            hyper::header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert_eq!(
            ctx.headers.get("X-Bin").map(String::as_str),
            Some(""),
            "a non-UTF-8 header value must remain present as empty, not be dropped"
        );
    }
}
