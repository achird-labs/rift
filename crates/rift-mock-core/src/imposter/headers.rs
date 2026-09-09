//! The one request-header collector both listeners use (issue #1025).

use std::collections::HashMap;
use std::hash::BuildHasher;

/// Flatten a request's headers into the `name -> [values]` map the imposter listener, the
/// intercept listener, and the rule/predicate matcher they both feed all share.
///
/// A repeated header keeps every value, in the order the client sent them (issue #994) — hyper's
/// `HeaderMap` iterator yields one `(name, value)` pair per occurrence, so appending rather than
/// `insert`-ing is what preserves them. A value that is not UTF-8 is dropped rather than passed
/// through `from_utf8_lossy` — matching/forwarding would otherwise see U+FFFD garbage the client
/// never sent. A name left with no surviving value is omitted entirely rather than kept with an
/// empty string, so `exists`/`equals ""` cannot be tricked into reporting a header the client
/// never actually sent readably (issue #1025).
///
/// `name` maps a header's wire name to the key stored in the map: Title-Case for the imposter
/// listener (Mountebank's `keyCaseSensitive` contract, issue #87), the identity function for the
/// intercept listener, which keeps hyper's already-lowercase names.
#[must_use]
pub fn collect_request_headers<SH: BuildHasher + Default>(
    headers: &hyper::HeaderMap,
    name: impl Fn(&str) -> String,
) -> HashMap<String, Vec<String>, SH> {
    // `keys_len` counts distinct names; `len()` counts values, which over-allocates for a request
    // carrying any repeated header.
    let mut out: HashMap<String, Vec<String>, SH> =
        HashMap::with_capacity_and_hasher(headers.keys_len(), SH::default());
    let mut dropped: Vec<String> = Vec::new();
    for (header_name, value) in headers {
        match value.to_str() {
            Ok(text) => {
                out.entry(name(header_name.as_str()))
                    .or_default()
                    .push(text.to_string());
            }
            // Dropped, not `from_utf8_lossy`-mangled (see above) — but that drop is a real
            // data-path swallow (the value becomes invisible to matching, forwarding, and
            // recording), so it is warned rather than merely debug-logged. Collected and emitted
            // once per request instead of once per header: which headers are non-UTF-8 is
            // entirely client-controlled, and a per-value warn on this path is an unbounded
            // log-volume lever for a hostile client (the shape #718 measured as a throughput
            // cost).
            // Named through the caller's `name` function, not raw off the wire: an operator
            // reading this warning is going to go looking for the header in `savedRequests` or in
            // a predicate, and those show the caller's key shape (Title-Case for imposters). The
            // allocation is confined to the Err arm, so a request that drops nothing pays nothing.
            Err(_) => dropped.push(name(header_name.as_str())),
        }
    }
    if !dropped.is_empty() {
        tracing::warn!(
            headers = %dropped.join(", "),
            "dropping non-UTF-8 request header value(s); they are invisible to matching, forwarding, and recording"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::FastMap;
    use hyper::header::HeaderValue;

    fn title_case(name: &str) -> String {
        crate::behaviors::header_to_title_case(name)
    }

    #[test]
    fn collector_keeps_every_value_in_send_order() {
        // Issue #1025: the imposter listener used to `collect()` into a single-value map, which is
        // last-wins, so a client sending the same name twice could only ever be matched on the
        // second value. Send order matters as much as the count: it is what `first()` at the
        // scripting and flow-id boundaries selects, and what proxy forwarding replays.
        let mut headers = hyper::HeaderMap::new();
        headers.append("x-test", HeaderValue::from_static("first"));
        headers.append("x-test", HeaderValue::from_static("second"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let collected: FastMap<String, Vec<String>> = collect_request_headers(&headers, title_case);

        assert_eq!(
            collected.get("X-Test").map(Vec::as_slice),
            Some(["first".to_string(), "second".to_string()].as_slice()),
            "both values, in the order the client sent them"
        );
        assert_eq!(
            collected.get("Content-Type").map(Vec::as_slice),
            Some(["application/json".to_string()].as_slice()),
            "a single-valued header is a one-element list, not a special case"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn collector_drops_non_utf8_values_and_omits_a_name_left_with_none() {
        // Issue #1025: `v.to_str().unwrap_or("")` turned a binary value into an empty string and
        // offered it to predicates, so `{"equals":{"headers":{"x-bin":""}}}` matched a request
        // that sent bytes. Dropping is the honest answer AND is what the intercept path already
        // does, so the two listeners agree.
        let non_utf8 =
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("a legal but non-UTF-8 header value");
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-bin", non_utf8.clone());
        headers.append("x-mixed", HeaderValue::from_static("readable"));
        headers.append("x-mixed", non_utf8);

        let collected: FastMap<String, Vec<String>> = collect_request_headers(&headers, title_case);

        assert!(
            !collected.contains_key("X-Bin"),
            "a name whose only value is non-UTF-8 is ABSENT, not present-with-an-empty-value — \
             otherwise `exists` is true and `equals \"\"` matches bytes the client never sent"
        );
        assert_eq!(
            collected.get("X-Mixed").map(Vec::as_slice),
            Some(["readable".to_string()].as_slice()),
            "the readable value of a partly-undecodable name survives, alone"
        );
        // The drop is data loss. What keeps it from being the very swallow this issue removes is
        // that it is announced — so the warning is part of the contract, not a nicety, and is
        // asserted rather than assumed. Both dropped names must be named.
        assert!(
            logs_contain("dropping non-UTF-8 request header value"),
            "a dropped value must be warned, not silently discarded"
        );
        assert!(
            logs_contain("X-Bin") && logs_contain("X-Mixed"),
            "the warning must name every header it dropped a value from, or an operator cannot \
             tell which of a request's headers went missing"
        );
    }

    #[test]
    fn collector_applies_the_callers_name_function() {
        // The imposter listener needs Title-Case keys (the `keyCaseSensitive` contract, issue #87)
        // while the intercept listener keeps hyper's already-lowercase names. One collector, two
        // name functions — which is what lets AC4 ("the two listeners agree") hold structurally.
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-my-header", HeaderValue::from_static("v"));

        let titled: FastMap<String, Vec<String>> = collect_request_headers(&headers, title_case);
        assert!(titled.contains_key("X-My-Header"), "imposter shape");

        let identity: FastMap<String, Vec<String>> =
            collect_request_headers(&headers, str::to_string);
        assert!(identity.contains_key("x-my-header"), "intercept shape");
    }

    #[test]
    fn flow_id_takes_the_first_value_of_a_repeated_header() {
        // Issue #1025 behaviour change: the live path used to resolve the flow id from the
        // single-value map, i.e. the LAST value of a repeated header. It now shares one
        // implementation with the recorded path, which has taken the FIRST value since #238 — so
        // a request and its own journal entry can no longer disagree about which space it is in.
        let config: crate::imposter::types::ImposterConfig =
            serde_json::from_value(serde_json::json!({
                "port": 0, "protocol": "http", "stubs": [],
                "_rift": { "flowState": { "flowIdSource": "header:X-Flow" } }
            }))
            .expect("config");
        let imposter = crate::imposter::Imposter::new(config).expect("imposter");

        let mut headers = hyper::HeaderMap::new();
        headers.append("x-flow", HeaderValue::from_static("tenant-a"));
        headers.append("x-flow", HeaderValue::from_static("tenant-b"));
        let collected: FastMap<String, Vec<String>> = collect_request_headers(&headers, title_case);

        assert_eq!(
            imposter.resolve_flow_id(&collected),
            "tenant-a",
            "the first value wins, matching resolve_flow_id_recorded (#238)"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn collector_returns_an_empty_map_for_a_request_with_no_headers() {
        let collected: FastMap<String, Vec<String>> =
            collect_request_headers(&hyper::HeaderMap::new(), title_case);
        assert!(collected.is_empty());
        assert!(
            !logs_contain("dropping non-UTF-8 request header value"),
            "a request that dropped nothing must not warn — an unconditional warning would be \
             noise on every request and would train an operator to ignore the real one"
        );
    }

    // Moved here from `rift-http-proxy`'s `intercept.rs` (issue #1025): that module's own
    // collector is now a thin wrapper over this one, so the shape it guarantees — every repeated
    // value kept, a non-UTF-8 value dropped — is pinned on the shared implementation instead.
    #[test]
    fn collect_request_headers_keeps_every_repeated_value_and_drops_non_utf8() {
        let mut headers = hyper::HeaderMap::new();
        headers.append("x-repeat", HeaderValue::from_static("first"));
        headers.append("x-repeat", HeaderValue::from_static("second"));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert(
            "x-binary",
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("a legal but non-UTF-8 header value"),
        );

        let collected: std::collections::HashMap<String, Vec<String>> =
            collect_request_headers(&headers, str::to_string);

        assert_eq!(
            collected.get("x-repeat").map(Vec::as_slice),
            Some(["first".to_string(), "second".to_string()].as_slice()),
            "a repeated header keeps every value, in send order (issue #994)"
        );
        assert_eq!(
            collected.get("content-type").map(Vec::as_slice),
            Some(["application/json".to_string()].as_slice()),
            "lookups stay case-insensitive because hyper's header names are already lowercase"
        );
        assert!(
            !collected.contains_key("x-binary"),
            "a non-UTF-8 value is dropped, not mangled into U+FFFD for predicates to match"
        );
    }
}
