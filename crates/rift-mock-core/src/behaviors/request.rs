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
    /// Build the behavior-facing view of a request.
    ///
    /// `headers` is the map `collect_request_headers` already built for this request — Title-Case
    /// names, every value the client sent, non-UTF-8 values already dropped with one warning. This
    /// function only *projects* it to one value per name; it does not re-derive it from the raw
    /// `HeaderMap` (issue #1040). Deriving rather than re-collecting is what stops behaviors
    /// disagreeing with predicates, proxy forwarding, the journal and inject about what the client
    /// sent: before, this was a second pass with a worse rule, coercing an undecodable value to
    /// `""` and keeping a repeated header's *last* value while every other surface took the first.
    ///
    /// A name is absent from the result when it carries no values, never present-but-empty — that
    /// conflation is the defect, not an edge case of it.
    ///
    /// **The caller owns the key casing.** This used to title-case internally, so it was correct
    /// for any input; it now passes keys straight through, which makes it correct only for a map
    /// collected with [`header_to_title_case`]. That matters because a same-typed map with
    /// *lowercase* keys exists one crate away — the intercept listener calls the same collector
    /// with `str::to_string` — and handing that one over would compile and then quietly give
    /// `decorate` and `MB_REQUEST` lowercase header names. `copy` and `lookup` would survive it
    /// (they scan case-insensitively), so the breakage would be partial and silent. Pass the
    /// imposter handler's map.
    pub fn from_request<SH: std::hash::BuildHasher>(
        method: &str,
        uri: &hyper::Uri,
        headers: &HashMap<String, Vec<String>, SH>,
        body: Option<&str>,
    ) -> Self {
        let mut query_map = HashMap::new();
        if let Some(query) = uri.query() {
            for pair in query.split('&').filter(|s| !s.is_empty()) {
                let (key, value) = match pair.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (pair, ""),
                };
                let decoded_key = crate::util::decode_or_raw(key);
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

        // First value, not last: this is the rule predicates state
        // (docs/mountebank/predicates.md), the rule `parse_form_data` applies to Content-Type
        // (issue #1038), and the rule the inject path already applied to the very same map — so
        // taking the last one here made one request answer two ways depending on which surface
        // read it.
        let header_map = headers
            .iter()
            .filter_map(|(name, values)| values.first().map(|first| (name.clone(), first.clone())))
            .collect();

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
        let ctx = RequestContext::from_request("GET", &uri, &HashMap::new(), None);
        assert_eq!(
            ctx.query.get("k").map(String::as_str),
            Some("%FF"),
            "an undecodable value must pass through raw, not become an empty string"
        );
    }

    #[test]
    fn from_request_still_decodes_a_valid_query_value() {
        let uri: hyper::Uri = "/p?k=hello%20world".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &HashMap::new(), None);
        assert_eq!(ctx.query.get("k").map(String::as_str), Some("hello world"));
    }

    // Issue #614: the key was left raw while the value was decoded, so a behavior reading the
    // request context saw a different parameter name than predicate matching did.
    #[test]
    fn from_request_decodes_an_encoded_query_key() {
        let uri: hyper::Uri = "/p?a%20b=1".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &HashMap::new(), None);
        assert_eq!(
            ctx.query.get("a b").map(String::as_str),
            Some("1"),
            "an encoded key must be decoded, matching imposter::parse_query_string"
        );
    }

    #[test]
    fn from_request_passes_through_an_undecodable_query_key() {
        let uri: hyper::Uri = "/p?%FF=v".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &HashMap::new(), None);
        assert_eq!(
            ctx.query.get("%FF").map(String::as_str),
            Some("v"),
            "an undecodable key must pass through raw, not collapse to an empty key"
        );
    }

    /// The collector's output shape: Title-Case names, every value the client sent, in order.
    fn collected(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, vs)| {
                (
                    header_to_title_case(k),
                    vs.iter().map(|v| (*v).to_string()).collect(),
                )
            })
            .collect()
    }

    // Issue #480 established that context keys are Title-Case. Since #1040 the casing is applied
    // once, by `collect_request_headers`, and `from_request` passes it through — so this now guards
    // that the projection does not re-case or otherwise disturb the collector's key shape.
    #[test]
    fn from_request_preserves_the_collectors_title_case_keys() {
        let headers = collected(&[
            ("content-type", &["application/json"]),
            ("x-custom-header", &["v"]),
        ]);
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert_eq!(
            ctx.headers.get("Content-Type").map(String::as_str),
            Some("application/json"),
            "header names reaching a behavior must be Title-Case"
        );
        assert_eq!(
            ctx.headers.get("X-Custom-Header").map(String::as_str),
            Some("v")
        );
    }

    // Issue #1040: this inverts the pre-#1040 rule. A value that is not valid UTF-8 must reach a
    // behavior as an ABSENT name, not as `""` — an empty string the client never sent, which made
    // `copy` substitute emptiness and `lookup` match an empty key, silently.
    //
    // Driven through the real `collect_request_headers` from a real `HeaderMap` rather than a
    // hand-built map: `from_request` alone can never observe raw bytes (it is handed `String`s), so
    // asserting the absence against a map that simply omits the name would pass against any
    // implementation at all — including one that took the last value. The guarantee only means
    // something as a property of the composed path, so that is what this exercises.
    #[test]
    fn a_header_the_collector_dropped_is_absent_from_the_behavior_context() {
        let mut raw = hyper::HeaderMap::new();
        raw.insert(
            hyper::header::HeaderName::from_static("x-bin"),
            hyper::header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        raw.insert(
            hyper::header::HeaderName::from_static("x-id"),
            hyper::header::HeaderValue::from_static("a"),
        );
        let headers: HashMap<String, Vec<String>> =
            crate::imposter::headers::collect_request_headers(&raw, header_to_title_case);
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert!(
            !ctx.headers.contains_key("X-Bin"),
            "a dropped header must be absent, not present as an empty string"
        );
        assert_eq!(
            ctx.headers.get("X-Id").map(String::as_str),
            Some("a"),
            "the decodable header alongside it is unaffected"
        );
    }

    // The same composed path, for the value-selection half: two values on the wire, first wins.
    // Pinning it here as well as on the hand-built map catches a collector that stopped preserving
    // send order, which would silently change which value every behavior sees.
    #[test]
    fn a_repeated_header_reaches_the_behavior_context_as_its_first_wire_value() {
        let mut raw = hyper::HeaderMap::new();
        raw.append(
            hyper::header::HeaderName::from_static("x-id"),
            hyper::header::HeaderValue::from_static("a"),
        );
        raw.append(
            hyper::header::HeaderName::from_static("x-id"),
            hyper::header::HeaderValue::from_static("b"),
        );
        let headers: HashMap<String, Vec<String>> =
            crate::imposter::headers::collect_request_headers(&raw, header_to_title_case);
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert_eq!(ctx.headers.get("X-Id").map(String::as_str), Some("a"));
    }

    // Issue #1040: `HashMap::insert` was last-wins, so a repeated header exposed its LAST value to
    // behaviors while predicates, `parse_form_data` (#1038) and the inject path all used the FIRST.
    // One request, two answers. First wins everywhere now.
    #[test]
    fn from_request_takes_the_first_value_of_a_repeated_header() {
        let headers = collected(&[("x-id", &["a", "b", "c"])]);
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert_eq!(
            ctx.headers.get("X-Id").map(String::as_str),
            Some("a"),
            "first value, matching predicates and the inject path"
        );
    }

    // A name mapped to no values cannot come from the collector, but `RequestContext` is public
    // API and an embedder can build the map by hand. It must read as absent — never as `""`, which
    // is the exact confusion #1040 removes.
    #[test]
    fn from_request_treats_a_name_with_no_values_as_absent() {
        let headers: HashMap<String, Vec<String>> =
            HashMap::from([("X-Empty".to_string(), Vec::new())]);
        let uri: hyper::Uri = "/p".parse().unwrap();

        let ctx = RequestContext::from_request("GET", &uri, &headers, None);

        assert!(!ctx.headers.contains_key("X-Empty"));
    }

    #[test]
    fn from_request_with_no_headers_yields_an_empty_map() {
        let uri: hyper::Uri = "/p".parse().unwrap();
        let ctx = RequestContext::from_request("GET", &uri, &HashMap::new(), None);
        assert!(ctx.headers.is_empty());
    }
}
