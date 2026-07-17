//! Predicate matching logic for Mountebank-compatible stub matching.
//!
//! Supports: equals, deepEquals, contains, startsWith, endsWith, matches, exists, not, or, and
//! Also supports requestFrom, ip, and form fields.

use crate::behaviors::{
    LazyXmlDom, eval_xpath_on, extract_jsonpath, extract_jsonpath_value, extract_xpath_with_ns,
};
use crate::imposter::types::{Predicate, PredicateOperation, PredicateSelector};
use crate::util::FastMap;
use std::collections::HashMap;
use std::hash::BuildHasher;

/// Check if a stub matches a request based on its predicates.
///
/// `Err` propagates a predicate-`inject` failure (issue #440: object-build failure or the
/// script itself throwing) — Mountebank fails the request loud on this rather than treating the
/// predicate as non-matching, so callers must surface the error rather than swallow it into
/// `false`. Every other predicate op is infallible.
#[allow(clippy::too_many_arguments)]
pub fn stub_matches<SH>(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String, SH>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Kept as the plain std-hasher map (unlike `headers`): a bare `None` here — the overwhelming
    // majority of calls — can't be threaded through a generic hasher parameter (nothing at the
    // call site would pin the type down), so `stub_matches`/`predicate_matches` keep this
    // parameter exactly as before and convert once at the `stub_matches_inner` boundary below.
    form: Option<&HashMap<String, String>>,
    imposter_port: u16,
) -> anyhow::Result<bool>
where
    SH: BuildHasher,
{
    // Parse the body and query once for standalone callers; the request hot path parses each once
    // per request (before the stub scan) and calls `stub_matches_inner` directly (issues #290, #480).
    let body_json = body.and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
    let query_map = parse_query(query);
    let form_fast: Option<FastMap<String, String>> =
        form.map(|f| f.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    // Standalone callers of this wrapper aren't a per-request hot loop, so there's no shared DOM
    // to thread in — `None` here just means an XPath predicate falls back to its own per-call parse
    // (correct, just not cached; see `stub_matches_inner`).
    stub_matches_inner(
        predicates,
        method,
        path,
        query,
        headers,
        body,
        request_from,
        client_ip,
        form_fast.as_ref(),
        imposter_port,
        body_json.as_ref(),
        None,
        Some(&query_map),
    )
}

/// Body of [`stub_matches`] taking the once-parsed request body so every predicate reuses one
/// parse rather than re-parsing the body per predicate and per stub (issue #290).
///
/// See [`stub_matches`] for the `Err` contract (issue #440).
#[allow(clippy::too_many_arguments)]
pub(crate) fn stub_matches_inner<SH>(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String, SH>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — always sourced from `Imposter::parse_form_data` (issue #704) on the
    // request hot path; unlike `headers`, this can't be generic (a bare `None` form argument, the
    // common non-form-predicate case, gives type inference nothing to pin the hasher to).
    form: Option<&FastMap<String, String>>,
    imposter_port: u16,
    body_json: Option<&serde_json::Value>,
    // The once-per-request lazily-parsed XML DOM (issue #711): `Some` on the request hot path
    // (`find_matching_stub_with_client`/`_linear`), which constructs one `LazyXmlDom` before the
    // stub loop and shares it across every stub's XPath predicates, so the body is parsed at most
    // once per request no matter how many XPath predicates evaluate it. `None` for standalone
    // callers (the `stub_matches` wrapper, tests) — they fall back to a per-call parse.
    xml_dom: Option<&LazyXmlDom<'_>>,
    // Always sourced from `parse_query`/`parse_query_string` (issue #704); same reasoning as `form`.
    query_map: Option<&FastMap<String, String>>,
) -> anyhow::Result<bool>
where
    SH: BuildHasher,
{
    // If no predicates, match everything
    if predicates.is_empty() {
        return Ok(true);
    }

    // All predicates must match (implicit AND)
    for predicate in predicates {
        if !predicate_matches_inner(
            predicate,
            method,
            path,
            query,
            headers,
            body,
            request_from,
            client_ip,
            form,
            imposter_port,
            body_json,
            xml_dom,
            query_map,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Parse query string for predicate matching, URL-decoding both keys and values
pub fn parse_query(query: Option<&str>) -> FastMap<String, String> {
    query.map_or_else(FastMap::default, parse_query_string)
}

// Allocation-free ASCII case-insensitive string tests (issue #480). The previous
// `haystack.to_lowercase().contains(&needle.to_lowercase())` allocated a lowercase copy of BOTH
// sides on every evaluation — the haystack can be the whole request body. These fold only ASCII
// A–Z/a–z, matching `str_equals`' existing `eq_ignore_ascii_case`, so the whole predicate engine
// is now consistently ASCII-case-insensitive rather than mixing ASCII equals with Unicode contains.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h[..n.len()].eq_ignore_ascii_case(n)
}

fn ends_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h[h.len() - n.len()..].eq_ignore_ascii_case(n)
}

/// Check if a single predicate matches (Mountebank-compatible)
/// Supports: equals, deepEquals, contains, startsWith, endsWith, matches, exists, not, or, and
/// Also supports requestFrom, ip, and form fields
///
/// `Err` propagates a predicate-`inject` failure (issue #440) — see [`stub_matches`] for the
/// full contract. Every other predicate op is infallible.
#[allow(clippy::too_many_arguments)]
pub fn predicate_matches<SH>(
    predicate: &Predicate,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String, SH>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // See `stub_matches` for why this stays the plain std-hasher map.
    form: Option<&HashMap<String, String>>,
    imposter_port: u16,
) -> anyhow::Result<bool>
where
    SH: BuildHasher,
{
    // Standalone callers parse the body and query here; the request hot path parses each once per
    // request and calls `predicate_matches_inner` directly with the shared parses (issues #290, #480).
    let body_json = body.and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
    let query_map = parse_query(query);
    let form_fast: Option<FastMap<String, String>> =
        form.map(|f| f.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    // Not the request hot loop — no shared DOM to thread in (see `stub_matches` for the same
    // reasoning); an XPath predicate degrades to its own per-call parse.
    predicate_matches_inner(
        predicate,
        method,
        path,
        query,
        headers,
        body,
        request_from,
        client_ip,
        form_fast.as_ref(),
        imposter_port,
        body_json.as_ref(),
        None,
        Some(&query_map),
    )
}

/// Body of [`predicate_matches`] taking the once-parsed request body (`body_json`) so it is not
/// re-parsed per predicate/stub. `body_json` is the parse of the raw request body; it is only
/// used for a no-selector predicate's `body` field (a selector makes the effective body differ).
///
/// See [`stub_matches`] for the `Err` contract (issue #440).
#[allow(clippy::too_many_arguments)]
// `imposter_port` is consumed by the `inject` predicate evaluator, which is `javascript`-gated
// (issue #599); without that feature the arm doesn't use it, so it's only threaded through the
// recursive And/Or descent — a false positive for this build only.
#[cfg_attr(not(feature = "javascript"), allow(clippy::only_used_in_recursion))]
pub(crate) fn predicate_matches_inner<SH>(
    predicate: &Predicate,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String, SH>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `stub_matches_inner`.
    form: Option<&FastMap<String, String>>,
    imposter_port: u16,
    body_json: Option<&serde_json::Value>,
    // The once-per-request lazily-parsed XML DOM (issue #711) — see `stub_matches_inner`.
    xml_dom: Option<&LazyXmlDom<'_>>,
    // Concretely `FastMap` — see `stub_matches_inner`.
    query_map: Option<&FastMap<String, String>>,
) -> anyhow::Result<bool>
where
    SH: BuildHasher,
{
    // Get predicate options
    let case_sensitive = predicate.parameters.case_sensitive.unwrap_or(false);

    // Get keyCaseSensitive option (defaults to caseSensitive value if not specified)
    let key_case_sensitive = predicate
        .parameters
        .key_case_sensitive
        .unwrap_or(case_sensitive);

    let except_pattern = Some(predicate.parameters.except.as_str()).filter(|s| !s.is_empty());
    // Compile the except pattern once (cached across requests) and reuse it for every field,
    // instead of recompiling per field per request. An invalid pattern yields `None`, which
    // preserves the previous fall-through-to-unchanged behavior.
    let except_regex = except_pattern.and_then(|pattern| cached_regex(pattern, false));

    // Helper to apply the except pattern. Borrows the input when no `except` is configured (the
    // common case) so a field comparison doesn't allocate a String per predicate (issue #294);
    // the except branch reuses `replace_all`'s native `Cow` instead of forcing an owned `String`.
    // Pin the higher-ranked bound so the closure infers `for<'a> Fn(&'a str) -> Cow<'a, str>`
    // (closures don't infer a borrowed return lifetime on their own).
    fn as_except_fn<F: for<'a> Fn(&'a str) -> std::borrow::Cow<'a, str>>(f: F) -> F {
        f
    }
    let apply_except = as_except_fn(|value| match &except_regex {
        Some(re) => re.replace_all(value, ""),
        None => std::borrow::Cow::Borrowed(value),
    });

    // Helper for string comparison with case sensitivity
    let str_equals = |expected: &str, actual: &str| -> bool {
        if case_sensitive {
            expected == actual
        } else {
            expected.eq_ignore_ascii_case(actual)
        }
    };

    let str_contains = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.contains(needle)
        } else {
            contains_ignore_ascii_case(haystack, needle)
        }
    };

    let str_starts_with = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.starts_with(needle)
        } else {
            starts_with_ignore_ascii_case(haystack, needle)
        }
    };

    let str_ends_with = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.ends_with(needle)
        } else {
            ends_with_ignore_ascii_case(haystack, needle)
        }
    };

    // Build request context for field access. Use the once-per-request parse when the caller
    // threaded it (hot path / standalone wrappers); only parse locally as a fallback (issue #480).
    let parsed_query;
    let query_map: &FastMap<String, String> = match query_map {
        Some(q) => q,
        None => {
            parsed_query = parse_query(query);
            &parsed_query
        }
    };
    let body_str = body.unwrap_or("");

    // Handle jsonpath parameter - extract value from JSON body
    let extracted_body: String;
    let effective_body = match &predicate.parameters.selector {
        Some(PredicateSelector::JsonPath { selector }) => {
            // Reuse the once-per-request body parse when the caller threaded it (issue #290) —
            // `extract_jsonpath_value` also reuses the process-wide compiled-selector cache
            // (issue #711) so this doesn't recompile `selector` per stub/predicate either way.
            extracted_body = match body_json {
                Some(json) => extract_jsonpath_value(json, selector),
                None => extract_jsonpath(body_str, selector),
            }
            .unwrap_or_default();
            &extracted_body
        }
        Some(PredicateSelector::XPath {
            selector,
            namespaces,
        }) => {
            // Reuse the once-per-request DOM when the caller threaded it (issue #711) — the DOM is
            // parsed at most once no matter how many XPath predicates/stubs evaluate it this request.
            extracted_body = match xml_dom.and_then(LazyXmlDom::document) {
                Some(document) => eval_xpath_on(&document, selector, namespaces.as_ref()),
                None => extract_xpath_with_ns(body_str, selector, namespaces.as_ref()),
            }
            .unwrap_or_default();
            &extracted_body
        }
        None => body_str,
    };

    // The once-parsed body corresponds to `effective_body` only when there is no selector; with a
    // jsonpath/xpath selector the effective body is a different string, so don't reuse the parse.
    let field_body_json = if predicate.parameters.selector.is_none() {
        body_json
    } else {
        None
    };

    match &predicate.operation {
        PredicateOperation::Equals(fields) => Ok(check_predicate_fields(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            str_equals,
            false, // not deep equals
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::DeepEquals(fields) => Ok(check_predicate_fields(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            str_equals,
            true, // deep equals
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::Contains(fields) => Ok(check_predicate_fields(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_contains(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::StartsWith(fields) => Ok(check_predicate_fields(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_starts_with(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::EndsWith(fields) => Ok(check_predicate_fields(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_ends_with(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::Matches(fields) => Ok(check_predicate_fields_regex(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            &apply_except,
            case_sensitive,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
            field_body_json,
        )),
        PredicateOperation::Exists(fields) => Ok(check_exists_predicate(
            fields,
            method,
            path,
            query_map,
            headers,
            effective_body,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        )),
        PredicateOperation::Not(inner) => Ok(!predicate_matches_inner(
            inner,
            method,
            path,
            query,
            headers,
            body,
            request_from,
            client_ip,
            form,
            imposter_port,
            body_json,
            xml_dom,
            Some(query_map),
        )?),
        PredicateOperation::Or(children) => {
            // Short-circuits on the first match, like the old `.any()`; a predicate-inject error
            // (issue #440) encountered along the way propagates immediately rather than being
            // treated as "this branch didn't match".
            for p in children {
                if predicate_matches_inner(
                    p,
                    method,
                    path,
                    query,
                    headers,
                    body,
                    request_from,
                    client_ip,
                    form,
                    imposter_port,
                    body_json,
                    xml_dom,
                    Some(query_map),
                )? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        PredicateOperation::And(children) => {
            // Short-circuits on the first non-match, like the old `.all()`; an inject error
            // propagates immediately (issue #440).
            for p in children {
                if !predicate_matches_inner(
                    p,
                    method,
                    path,
                    query,
                    headers,
                    body,
                    request_from,
                    client_ip,
                    form,
                    imposter_port,
                    body_json,
                    xml_dom,
                    Some(query_map),
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        PredicateOperation::Inject(inject_fn) => {
            #[cfg(feature = "javascript")]
            {
                use crate::scripting::{MountebankRequest, execute_predicate_inject};
                let mb_request = MountebankRequest {
                    method: method.to_string(),
                    path: path.to_string(),
                    // Reuse the once-per-request query map instead of re-parsing (issue #480).
                    // `MountebankRequest`'s fields are the fixed std-hasher scripting boundary
                    // (out of scope for #704), so a hasher-changing map is copied across rather
                    // than moved.
                    query: query_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    headers: headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    // `body` is already the classified string from the caller (base64 for a
                    // binary request body, issue #636); this predicate-inject path doesn't thread
                    // the mode flag through separately, so default to `Text` — the script still
                    // sees the correct (base64) string, it just can't ask `isBinary`.
                    body: body.map(|b| b.to_string()),
                    mode: None,
                };
                execute_predicate_inject(inject_fn, &mb_request, imposter_port)
            }
            #[cfg(not(feature = "javascript"))]
            {
                tracing::warn!(
                    "inject predicate requires the 'javascript' feature; predicate will not match"
                );
                let _ = inject_fn;
                Ok(false)
            }
        }
    }
}

mod fields;
mod json;
pub(crate) mod regex_cache;
use fields::{check_predicate_fields, check_predicate_fields_regex};
use json::check_exists_predicate;
use regex_cache::cached_regex;

/// Parse query string into HashMap (public helper)
/// URL-decodes both keys and values to properly handle encoded characters.
/// Bare params without `=` (e.g. `?flag`) are treated as key with empty value.
/// Duplicate keys are joined with commas (Mountebank `stringify` behavior).
pub fn parse_query_string(query: &str) -> FastMap<String, String> {
    let mut map = FastMap::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let decoded_key = crate::util::decode_or_raw(key);
        let decoded_value = crate::util::decode_or_raw(value);
        map.entry(decoded_key)
            .and_modify(|existing: &mut String| {
                existing.push(',');
                existing.push_str(&decoded_value);
            })
            .or_insert(decoded_value);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::types::{Predicate, PredicateOperation, PredicateParameters};
    use serde_json::json;

    /// Helper to build a Predicate from operation with default parameters
    fn make_predicate(op: PredicateOperation) -> Predicate {
        Predicate {
            parameters: PredicateParameters::default(),
            operation: op,
        }
    }

    /// Helper to build a Predicate with custom parameters
    fn make_predicate_with_params(
        op: PredicateOperation,
        params: PredicateParameters,
    ) -> Predicate {
        Predicate {
            parameters: params,
            operation: op,
        }
    }

    fn empty_headers() -> HashMap<String, String> {
        HashMap::new()
    }

    // =========================================================================
    // Multi-valued query parameters (Issue #83 - fixed)
    // Multiple values for the same key are comma-joined: ?key=first&key=second → "first,second"
    // =========================================================================

    #[test]
    fn test_parse_query_string_multi_valued_comma_joined() {
        let result = parse_query_string("key=first&key=second");

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get("key"),
            Some(&"first,second".to_string()),
            "Multi-valued query params should be comma-joined"
        );
    }

    #[test]
    fn test_equals_query_multi_valued_param() {
        // equals { query: { key: "first,second" } } should match ?key=first&key=second
        let fields: HashMap<String, serde_json::Value> =
            [("query".to_string(), json!({"key": "first,second"}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            Some("key=first&key=second"),
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "equals should match comma-joined multi-valued query params"
        );
    }

    // =========================================================================
    // Bare query parameters without '=' sign (Issue #84 - fixed)
    // Mountebank treats ?flag as flag=""
    // =========================================================================

    #[test]
    fn test_parse_query_string_bare_param() {
        let result = parse_query_string("flag");

        assert_eq!(
            result.get("flag"),
            Some(&String::new()),
            "Bare query param 'flag' should be present with empty value"
        );
    }

    #[test]
    fn test_parse_query_string_mixed_bare_and_valued() {
        let result = parse_query_string("a=1&flag&b=2");

        assert_eq!(result.get("a"), Some(&"1".to_string()));
        assert_eq!(result.get("b"), Some(&"2".to_string()));
        assert!(
            result.contains_key("flag"),
            "Bare param 'flag' should be present in mixed query string"
        );
    }

    #[test]
    fn test_exists_query_bare_param() {
        let fields: HashMap<String, serde_json::Value> =
            [("query".to_string(), json!({"flag": true}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            Some("flag"),
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "exists predicate should match bare query param '?flag'"
        );
    }

    // =========================================================================
    // deepEquals body structural comparison (Issue #85 - fixed)
    // JSON objects are compared structurally, not by string representation
    // =========================================================================

    #[test]
    fn test_deep_equals_body_json_key_order_independence() {
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"a": 1, "b": 2}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::DeepEquals(fields));

        let body = r#"{"b":2,"a":1}"#;

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            None,
            &empty_headers(),
            Some(body),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "deepEquals should match JSON bodies regardless of key order"
        );
    }

    #[test]
    fn stub_with_two_json_body_predicates_shares_one_parse() {
        // Gate for #290: two predicates over the same JSON body in one stub (implicit AND) —
        // the body is parsed once and reused, and deepEquals + contains semantics are unchanged.
        let deep: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"a": 1, "b": 2}))]
                .into_iter()
                .collect();
        let contains_fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"a": 1}))]
                .into_iter()
                .collect();
        let predicates = vec![
            make_predicate(PredicateOperation::DeepEquals(deep)),
            make_predicate(PredicateOperation::Contains(contains_fields)),
        ];

        // Matching body satisfies both predicates.
        assert!(
            stub_matches(
                &predicates,
                "POST",
                "/x",
                None,
                &empty_headers(),
                Some(r#"{"b":2,"a":1}"#),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // A body that breaks deepEquals (extra key) must fail the AND.
        assert!(
            !stub_matches(
                &predicates,
                "POST",
                "/x",
                None,
                &empty_headers(),
                Some(r#"{"a":1,"b":2,"c":3}"#),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // Non-JSON body must not match a JSON-object predicate.
        assert!(
            !stub_matches(
                &predicates,
                "POST",
                "/x",
                None,
                &empty_headers(),
                Some("not json"),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn deep_equals_with_jsonpath_selector_uses_extracted_body_not_raw_parse() {
        // Gate for #290: a selector predicate must compare against the EXTRACTED effective body,
        // never the once-parsed raw body. With `$.data` extracting `{"a":1,"b":2}`, deepEquals
        // matches only if the extracted object (not the whole raw body, which also has `other`)
        // is what gets compared — so this asserts the `selector.is_none()` guard holds.
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"a": 1, "b": 2}))]
                .into_iter()
                .collect();
        let params = PredicateParameters {
            selector: Some(PredicateSelector::JsonPath {
                selector: "$.data".to_string(),
            }),
            ..PredicateParameters::default()
        };
        let pred = make_predicate_with_params(PredicateOperation::DeepEquals(fields), params);

        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/x",
                None,
                &empty_headers(),
                Some(r#"{"data":{"a":1,"b":2},"other":"x"}"#),
                None,
                None,
                None,
                0,
            )
            .unwrap(),
            "deepEquals over the jsonpath-extracted object must match; reusing the raw-body parse \
             (which also has `other`) would wrongly fail"
        );
    }

    #[test]
    fn test_deep_equals_body_string_match() {
        // deepEquals with matching string body should still work
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!("hello world"))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::DeepEquals(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            None,
            &empty_headers(),
            Some("hello world"),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(result, "deepEquals should match identical string bodies");
    }

    // =========================================================================
    // keyCaseSensitive in exists predicate (Issue #86 - fixed)
    // =========================================================================

    #[test]
    fn test_exists_query_key_case_sensitive_false() {
        // With keyCaseSensitive: false (default), exists { query: { Key: true } }
        // should match if query has "key" (case-insensitive key lookup)
        let fields: HashMap<String, serde_json::Value> =
            [("query".to_string(), json!({"Key": true}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            Some("key=value"),
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "exists with keyCaseSensitive=false should match case-insensitively"
        );
    }

    #[test]
    fn test_exists_header_key_case_sensitive_true() {
        // With keyCaseSensitive: true, exists { headers: { "Content-Type": true } }
        // should match header "Content-Type" (exact case match)
        let fields: HashMap<String, serde_json::Value> =
            [("headers".to_string(), json!({"Content-Type": true}))]
                .into_iter()
                .collect();

        let params = PredicateParameters {
            key_case_sensitive: Some(true),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Exists(fields), params);

        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let result = predicate_matches(
            &pred, "GET", "/test", None, &headers, None, None, None, None, 0,
        )
        .unwrap();

        assert!(
            result,
            "exists with keyCaseSensitive=true should match exact case header key"
        );
    }

    #[test]
    fn test_exists_form_key_case_sensitive() {
        // exists { form: { Name: true } } with keyCaseSensitive: false (default)
        // should match form with key "name"
        let fields: HashMap<String, serde_json::Value> =
            [("form".to_string(), json!({"Name": true}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let mut form = HashMap::new();
        form.insert("name".to_string(), "John".to_string());

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            Some("name=John"),
            None,
            None,
            Some(&form),
            0,
        )
        .unwrap();

        assert!(
            result,
            "exists with keyCaseSensitive=false should match form key case-insensitively"
        );
    }

    // =========================================================================
    // Header keys Title-Case normalization (Issue #87 - fixed)
    // Headers are now Title-Case (Content-Type, not content-type)
    // =========================================================================

    #[test]
    fn test_header_key_case_sensitive_true_with_title_case() {
        // With keyCaseSensitive: true, equals { headers: { "Content-Type": "application/json" } }
        // should match because headers are now Title-Case after fix #87
        let fields: HashMap<String, serde_json::Value> = [(
            "headers".to_string(),
            json!({"Content-Type": "application/json"}),
        )]
        .into_iter()
        .collect();

        let params = PredicateParameters {
            case_sensitive: Some(true),
            key_case_sensitive: Some(true),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Equals(fields), params);

        // After fix #87, header_map_to_hashmap() produces Title-Case keys
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let result = predicate_matches(
            &pred, "GET", "/test", None, &headers, None, None, None, None, 0,
        )
        .unwrap();

        assert!(
            result,
            "keyCaseSensitive=true with Title-Case header key should match"
        );
    }

    #[test]
    fn test_header_key_case_sensitive_false_default() {
        // Default: keyCaseSensitive=false, header key matching should be case-insensitive
        let fields: HashMap<String, serde_json::Value> = [(
            "headers".to_string(),
            json!({"Content-Type": "application/json"}),
        )]
        .into_iter()
        .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        // Even with lowercase keys, keyCaseSensitive=false means case-insensitive matching
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            None,
            &headers,
            Some(""),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "Default keyCaseSensitive=false should match case-insensitively"
        );
    }

    // =========================================================================
    // Additional predicate tests (non-bug, for coverage)
    // =========================================================================

    #[test]
    fn test_equals_method() {
        let fields: HashMap<String, serde_json::Value> = [("method".to_string(), json!("POST"))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/test",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // Default is case-insensitive
        assert!(
            predicate_matches(
                &pred,
                "post",
                "/test",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/test",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_equals_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api/users"))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/api/other",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_contains_body() {
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!("hello"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::Contains(fields));

        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/",
                None,
                &empty_headers(),
                Some("say hello world"),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "POST",
                "/",
                None,
                &empty_headers(),
                Some("goodbye"),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_starts_with_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api/"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::StartsWith(fields));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/web/page",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_ends_with_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!(".json"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::EndsWith(fields));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/data/file.json",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/data/file.xml",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_matches_regex() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("^/api/users/\\d+$"))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Matches(fields));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/api/users/123",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/api/users/abc",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_matches_regex_honors_case_sensitivity() {
        let fields: HashMap<String, serde_json::Value> = [("path".to_string(), json!("^/API/"))]
            .into_iter()
            .collect();

        // Default (caseSensitive unset → false): the cached regex is case-insensitive, so an
        // uppercase pattern matches a lowercase path.
        let insensitive = make_predicate(PredicateOperation::Matches(fields.clone()));
        assert!(
            predicate_matches(
                &insensitive,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );

        // caseSensitive: true → the cached regex is case-sensitive and must NOT match.
        let sensitive = make_predicate_with_params(
            PredicateOperation::Matches(fields),
            PredicateParameters {
                case_sensitive: Some(true),
                ..PredicateParameters::default()
            },
        );
        assert!(
            !predicate_matches(
                &sensitive,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_not_predicate() {
        let inner_fields: HashMap<String, serde_json::Value> =
            [("method".to_string(), json!("GET"))].into_iter().collect();

        let inner = make_predicate(PredicateOperation::Equals(inner_fields));
        let pred = make_predicate(PredicateOperation::Not(Box::new(inner)));

        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_or_predicate() {
        let eq_get: HashMap<String, serde_json::Value> =
            [("method".to_string(), json!("GET"))].into_iter().collect();
        let eq_post: HashMap<String, serde_json::Value> = [("method".to_string(), json!("POST"))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::Or(vec![
            make_predicate(PredicateOperation::Equals(eq_get)),
            make_predicate(PredicateOperation::Equals(eq_post)),
        ]));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "DELETE",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_and_predicate() {
        let eq_get: HashMap<String, serde_json::Value> =
            [("method".to_string(), json!("GET"))].into_iter().collect();
        let eq_path: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::And(vec![
            make_predicate(PredicateOperation::Equals(eq_get)),
            make_predicate(PredicateOperation::Equals(eq_path)),
        ]));

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/api",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "POST",
                "/api",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/other",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_case_sensitive_equals() {
        let fields: HashMap<String, serde_json::Value> = [("method".to_string(), json!("POST"))]
            .into_iter()
            .collect();

        let params = PredicateParameters {
            case_sensitive: Some(true),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Equals(fields), params);

        assert!(
            predicate_matches(
                &pred,
                "POST",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // With caseSensitive: true, "post" should NOT match "POST"
        assert!(
            !predicate_matches(
                &pred,
                "post",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_except_pattern() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api/users"))]
                .into_iter()
                .collect();

        let params = PredicateParameters {
            except: "/api".to_string(),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Equals(fields), params);

        // except removes "/api" from actual path, so "/api/users" becomes "/users"
        // which doesn't match "/api/users"
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_exists_body() {
        let fields_true: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!(true))].into_iter().collect();

        let fields_false: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!(false))].into_iter().collect();

        let pred_true = make_predicate(PredicateOperation::Exists(fields_true));
        let pred_false = make_predicate(PredicateOperation::Exists(fields_false));

        // Body exists
        assert!(
            predicate_matches(
                &pred_true,
                "POST",
                "/",
                None,
                &empty_headers(),
                Some("content"),
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // Body does not exist
        assert!(
            !predicate_matches(
                &pred_true,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // Body should NOT exist (false) - empty body
        assert!(
            predicate_matches(
                &pred_false,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_exists_header() {
        let fields: HashMap<String, serde_json::Value> =
            [("headers".to_string(), json!({"content-type": true}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        assert!(
            predicate_matches(&pred, "GET", "/", None, &headers, None, None, None, None, 0)
                .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_deep_equals_query_extra_params_mismatch() {
        // deepEquals on query should fail if actual has extra params
        let fields: HashMap<String, serde_json::Value> = [("query".to_string(), json!({"a": "1"}))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::DeepEquals(fields));

        // Exact match - should pass
        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/",
                Some("a=1"),
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        // Extra param - should fail for deepEquals
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/",
                Some("a=1&b=2"),
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_deep_equals_headers_extra_mismatch() {
        // deepEquals on headers should fail if actual has extra headers
        let fields: HashMap<String, serde_json::Value> =
            [("headers".to_string(), json!({"x-custom": "value"}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::DeepEquals(fields));

        let mut exact_headers = HashMap::new();
        exact_headers.insert("x-custom".to_string(), "value".to_string());

        let mut extra_headers = HashMap::new();
        extra_headers.insert("x-custom".to_string(), "value".to_string());
        extra_headers.insert("x-other".to_string(), "other".to_string());

        assert!(
            predicate_matches(
                &pred,
                "GET",
                "/",
                None,
                &exact_headers,
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
        assert!(
            !predicate_matches(
                &pred,
                "GET",
                "/",
                None,
                &extra_headers,
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn test_stub_matches_empty_predicates() {
        // Empty predicates should match everything
        assert!(
            stub_matches(
                &[],
                "GET",
                "/anything",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0
            )
            .unwrap()
        );
    }

    #[test]
    fn test_stub_matches_all_must_match() {
        // All predicates must match (implicit AND)
        let predicates = vec![
            make_predicate(PredicateOperation::Equals(
                [("method".to_string(), json!("GET"))].into_iter().collect(),
            )),
            make_predicate(PredicateOperation::Equals(
                [("path".to_string(), json!("/api"))].into_iter().collect(),
            )),
        ];

        assert!(
            stub_matches(
                &predicates,
                "GET",
                "/api",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0
            )
            .unwrap()
        );
        assert!(
            !stub_matches(
                &predicates,
                "POST",
                "/api",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0
            )
            .unwrap()
        );
    }

    #[test]
    fn test_parse_query_string_url_encoded() {
        // Already fixed in #70 - verify URL decoding works
        let result = parse_query_string("key=hello%20world&name=caf%C3%A9");
        assert_eq!(result.get("key"), Some(&"hello world".to_string()));
        assert_eq!(result.get("name"), Some(&"café".to_string()));
    }

    #[test]
    fn test_parse_query_string_empty() {
        let result = parse_query_string("");
        assert!(result.is_empty());
    }

    // Issue #611: an undecodable percent-sequence must pass through raw rather than blank the key
    // or value. Blanking destroys matchable text and, on the key, collapses distinct params into a
    // single "" entry that then comma-joins unrelated values.
    #[test]
    fn test_parse_query_string_passes_through_undecodable_sequences() {
        let value = parse_query_string("k=%FF");
        assert_eq!(
            value.get("k"),
            Some(&"%FF".to_string()),
            "an undecodable value must pass through raw, not become empty"
        );

        let key = parse_query_string("%FF=v");
        assert_eq!(
            key.get("%FF"),
            Some(&"v".to_string()),
            "an undecodable key must pass through raw, not collapse to an empty key"
        );

        // The collision the fix exists to prevent: two *different* undecodable keys both blanked
        // to "" used to comma-join their unrelated values into a single bogus entry.
        let collision = parse_query_string("%FF=a&%FE=b");
        assert_eq!(collision.get("%FF"), Some(&"a".to_string()));
        assert_eq!(collision.get("%FE"), Some(&"b".to_string()));
        assert_eq!(
            collision.len(),
            2,
            "distinct undecodable keys must stay distinct, not collapse into one \"\" entry"
        );
    }

    // Fix #104: JSON body key matching now respects keyCaseSensitive
    #[test]
    fn test_equals_json_body_key_case_insensitive_by_default() {
        // equals { body: { "Name": "John" } } should match body {"name": "John"}
        // because caseSensitive defaults to false, which affects JSON body keys too.
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"Name": "John"}))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            Some(r#"{"name": "John"}"#),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "caseSensitive=false (default) should match JSON body keys case-insensitively"
        );
    }

    // Fix #105: except is now applied to leaf values inside compare_json_recursive,
    // not to the raw JSON string before parsing.
    #[test]
    fn test_except_applied_to_leaf_values_not_raw_json() {
        // equals { body: { "greeting": "hello" } } with except="\\d+"
        // Body: {"greeting": "hello", "count": 42}
        // except strips digits from leaf values after parsing, not from raw JSON.
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!({"greeting": "hello"}))]
                .into_iter()
                .collect();

        let params = PredicateParameters {
            except: "\\d+".to_string(),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Equals(fields), params);

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            Some(r#"{"greeting": "hello", "count": 42}"#),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "except should apply to leaf values after JSON parsing, not break raw JSON structure"
        );
    }

    // Fix #106: Invalid regex patterns now cause the predicate to not match
    #[test]
    fn test_matches_invalid_regex_returns_false() {
        let fields: HashMap<String, serde_json::Value> = [("path".to_string(), json!("[unclosed"))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::Matches(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/any/path/at/all",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            !result,
            "Invalid regex pattern should cause the predicate to not match"
        );
    }

    // Fix #99: except is now applied to method in matches predicate
    #[test]
    fn test_matches_method_with_except_applied() {
        // matches { method: "^OST$" } with except="P" and method "POST"
        // After applying except: "POST" → "OST", regex "^OST$" matches "OST" → true
        let fields: HashMap<String, serde_json::Value> = [("method".to_string(), json!("^OST$"))]
            .into_iter()
            .collect();

        let params = PredicateParameters {
            except: "P".to_string(),
            ..Default::default()
        };

        let pred = make_predicate_with_params(PredicateOperation::Matches(fields), params);

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "except should be applied to method before regex matching"
        );
    }

    // Fix #100: equals array matching now checks array length
    #[test]
    fn test_equals_body_array_rejects_longer_actual() {
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!([1, 2]))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            Some("[1, 2, 3]"),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            !result,
            "equals [1, 2] should not match [1, 2, 3] — arrays have different lengths"
        );
    }

    #[test]
    fn test_equals_body_array_rejects_shorter_actual() {
        let fields: HashMap<String, serde_json::Value> = [("body".to_string(), json!([1, 2, 3]))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        let result = predicate_matches(
            &pred,
            "POST",
            "/test",
            None,
            &empty_headers(),
            Some("[1, 2]"),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            !result,
            "equals [1, 2, 3] should not match [1, 2] — arrays have different lengths"
        );
    }

    // Fix #101: exists predicate now handles method, path, requestFrom, ip
    #[test]
    fn test_exists_method_false_fails_when_present() {
        let fields: HashMap<String, serde_json::Value> =
            [("method".to_string(), json!(false))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            !result,
            "exists method=false should fail when method is present"
        );
    }

    #[test]
    fn test_exists_path_false_fails_when_present() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!(false))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::Exists(fields));

        let result = predicate_matches(
            &pred,
            "GET",
            "/test",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            !result,
            "exists path=false should fail when path is present"
        );
    }

    // Issue #306: a bare jsonpath selector (no leading `$`) must match, just like
    // the rooted form `$.searchValue`.
    #[test]
    fn test_jsonpath_bare_selector_predicate_matches() {
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!("v"))].into_iter().collect();
        let params = PredicateParameters {
            selector: Some(PredicateSelector::JsonPath {
                selector: "searchValue".to_string(),
            }),
            ..PredicateParameters::default()
        };
        let pred = make_predicate_with_params(PredicateOperation::Equals(fields), params);

        let result = predicate_matches(
            &pred,
            "POST",
            "/x",
            None,
            &empty_headers(),
            Some(r#"{"searchValue":"v"}"#),
            None,
            None,
            None,
            0,
        )
        .unwrap();

        assert!(
            result,
            "bare jsonpath selector should match like $.searchValue"
        );
    }

    // =========================================================================
    // inject predicate tests (require javascript feature)
    // =========================================================================

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_matches_true() {
        let pred = make_predicate(PredicateOperation::Inject(
            "function(request) { return request.path === '/api'; }".to_string(),
        ));
        let result = predicate_matches(
            &pred,
            "GET",
            "/api",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();
        assert!(result, "inject predicate returning true should match");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_matches_false() {
        let pred = make_predicate(PredicateOperation::Inject(
            "function(request) { return request.path === '/other'; }".to_string(),
        ));
        let result = predicate_matches(
            &pred,
            "GET",
            "/api",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();
        assert!(!result, "inject predicate returning false should not match");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_checks_method() {
        let pred = make_predicate(PredicateOperation::Inject(
            "function(request) { return request.method === 'POST'; }".to_string(),
        ));
        let headers = HashMap::new();
        let post_result = predicate_matches(
            &pred, "POST", "/", None, &headers, None, None, None, None, 0,
        )
        .unwrap();
        let get_result =
            predicate_matches(&pred, "GET", "/", None, &headers, None, None, None, None, 0)
                .unwrap();
        assert!(post_result, "inject predicate should match POST");
        assert!(!get_result, "inject predicate should not match GET");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_accesses_body() {
        let pred = make_predicate(PredicateOperation::Inject(
            r#"function(request) { return request.body && request.body.indexOf('hello') >= 0; }"#
                .to_string(),
        ));
        let result = predicate_matches(
            &pred,
            "POST",
            "/",
            None,
            &empty_headers(),
            Some("hello world"),
            None,
            None,
            None,
            0,
        )
        .unwrap();
        assert!(result, "inject predicate should access request body");
    }

    #[test]
    fn test_inject_predicate_deserializes() {
        let json = r#"{"inject": "function(request) { return true; }"}"#;
        let op: PredicateOperation = serde_json::from_str(json).expect("should deserialize inject");
        assert!(matches!(op, PredicateOperation::Inject(_)));
    }

    // =========================================================================
    // Issue #440: a predicate `inject` that throws/errors must fail loud (Err), not silently
    // collapse to "didn't match" (Ok(false)) — matching Mountebank's `InjectionError` behavior.
    // =========================================================================

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_throwing_script_fails_loud() {
        let pred = make_predicate(PredicateOperation::Inject(
            "function(request) { throw new Error('boom'); }".to_string(),
        ));
        let result = predicate_matches(
            &pred,
            "GET",
            "/api",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        );
        assert!(
            result.is_err(),
            "a throwing inject predicate must return Err, not Ok(false)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid predicate injection"),
            "unexpected error message: {err}"
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_undefined_reference_fails_loud() {
        let pred = make_predicate(PredicateOperation::Inject(
            "function(request) { return someUndefinedVariable.path === request.path; }".to_string(),
        ));
        let result = predicate_matches(
            &pred,
            "GET",
            "/api",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        );
        assert!(
            result.is_err(),
            "a reference-error inject predicate must return Err, not Ok(false)"
        );
    }

    // Not/Or/And must propagate an inject error from a nested predicate rather than swallow it
    // into the boolean combinator result.
    #[cfg(feature = "javascript")]
    #[test]
    fn test_inject_predicate_error_propagates_through_or_and_not() {
        let throwing = || {
            make_predicate(PredicateOperation::Inject(
                "function(request) { throw new Error('boom'); }".to_string(),
            ))
        };
        let always_true = || make_predicate(PredicateOperation::Exists(HashMap::new()));

        let or_pred = make_predicate(PredicateOperation::Or(vec![throwing(), always_true()]));
        assert!(
            predicate_matches_inner(
                &or_pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
                None,
                None,
                None,
            )
            .is_err(),
            "an `or` must propagate a nested inject error rather than short-circuit past it"
        );

        let and_pred = make_predicate(PredicateOperation::And(vec![always_true(), throwing()]));
        assert!(
            predicate_matches_inner(
                &and_pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
                None,
                None,
                None,
            )
            .is_err(),
            "an `and` must propagate a nested inject error"
        );

        let not_pred = make_predicate(PredicateOperation::Not(Box::new(throwing())));
        assert!(
            predicate_matches_inner(
                &not_pred,
                "GET",
                "/",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
                None,
                None,
                None,
            )
            .is_err(),
            "a `not` must propagate a nested inject error rather than negate it into a match"
        );
    }

    // stub_matches (used by the request hot path via stub_matches_inner) must not swallow a
    // predicate-inject error into "stub didn't match" — it must propagate so the caller can fail
    // the request loud (issue #440).
    #[cfg(feature = "javascript")]
    #[test]
    fn test_stub_matches_propagates_predicate_inject_error() {
        let predicates = vec![make_predicate(PredicateOperation::Inject(
            "function(request) { throw new Error('boom'); }".to_string(),
        ))];
        let result = stub_matches(
            &predicates,
            "GET",
            "/api",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        );
        assert!(
            result.is_err(),
            "stub_matches must propagate a predicate-inject error, not collapse it to false"
        );
    }

    // Issue #480 — the query is now parsed once per request and threaded into predicate matching.
    // Passing the pre-parsed map (Some) must give the identical result as the local-parse fallback
    // (None); the optimization is behavior-preserving.
    #[test]
    fn query_hoist_matches_local_parse() {
        let fields: HashMap<String, serde_json::Value> =
            [("query".to_string(), json!({ "key": "value" }))]
                .into_iter()
                .collect();
        let pred = make_predicate(PredicateOperation::Equals(fields));
        let query = Some("key=value");
        let parsed = parse_query(query);

        let with_hoisted = predicate_matches_inner(
            &pred,
            "GET",
            "/test",
            query,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            Some(&parsed),
        )
        .unwrap();
        let with_fallback = predicate_matches_inner(
            &pred,
            "GET",
            "/test",
            query,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
        )
        .unwrap();

        assert!(
            with_hoisted,
            "query predicate must match via the threaded map"
        );
        assert_eq!(
            with_hoisted, with_fallback,
            "threaded map and local-parse fallback must agree"
        );
    }

    // Issue #480: the resolved query map must reach predicates nested inside Or/And/Not, which
    // recurse through `predicate_matches_inner`. A query predicate inside an `or` must still match.
    #[test]
    fn nested_query_predicate_in_or_matches() {
        let query_eq: HashMap<String, serde_json::Value> =
            [("query".to_string(), json!({ "k": "v" }))]
                .into_iter()
                .collect();
        let never: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/nope"))].into_iter().collect();
        let or_pred = make_predicate(PredicateOperation::Or(vec![
            make_predicate(PredicateOperation::Equals(never)),
            make_predicate(PredicateOperation::Equals(query_eq)),
        ]));

        let matched = predicate_matches(
            &or_pred,
            "GET",
            "/test",
            Some("k=v"),
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap();
        assert!(
            matched,
            "a query predicate nested in `or` must see the threaded query map"
        );
    }

    // Issue #480 — the case-insensitive string helpers are allocation-free ASCII folds. They must
    // behave like the old `to_lowercase()` path for ASCII input, including edge cases.
    #[test]
    fn ascii_case_insensitive_helpers() {
        // contains
        assert!(contains_ignore_ascii_case("Hello World", "LO WO"));
        assert!(contains_ignore_ascii_case("anything", "")); // empty needle matches
        assert!(!contains_ignore_ascii_case("short", "longer needle"));
        assert!(!contains_ignore_ascii_case("abc", "xyz"));
        // starts_with / ends_with
        assert!(starts_with_ignore_ascii_case("Content-Type", "content-"));
        assert!(!starts_with_ignore_ascii_case("abc", "abcd"));
        assert!(ends_with_ignore_ascii_case("image.PNG", ".png"));
        assert!(!ends_with_ignore_ascii_case("abc", "zabc"));
        // non-ASCII bytes fold only ASCII: exact non-ASCII still matches, cross-case non-ASCII does not
        assert!(contains_ignore_ascii_case("caf\u{00e9}", "CAF\u{00e9}"));
    }

    // Issue #480 — predicate-level case-insensitive contains/startsWith/endsWith still match with
    // mixed ASCII case after switching to the allocation-free path.
    #[test]
    fn case_insensitive_string_predicates_still_match() {
        let ci = PredicateParameters {
            case_sensitive: Some(false),
            ..PredicateParameters::default()
        };
        let run = |op: PredicateOperation| {
            let pred = make_predicate_with_params(op, ci.clone());
            predicate_matches(
                &pred,
                "GET",
                "/API/Users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
            )
            .unwrap()
        };
        let path_field = |needle: &str| -> HashMap<String, serde_json::Value> {
            [("path".to_string(), json!(needle))].into_iter().collect()
        };

        assert!(run(PredicateOperation::Contains(path_field("api/users"))));
        assert!(run(PredicateOperation::StartsWith(path_field("/api"))));
        assert!(run(PredicateOperation::EndsWith(path_field("USERS"))));
    }
}
