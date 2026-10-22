//! Predicate matching logic for Mountebank-compatible stub matching.
//!
//! Supports: equals, deepEquals, contains, startsWith, endsWith, matches, exists, not, or, and
//! Also supports requestFrom, ip, and form fields.

use crate::behaviors::{
    LazyJson, LazyXmlDom, Selection, fold_xpath_selector, jsonpath_selection, xpath_selection,
};
use crate::imposter::types::{Predicate, PredicateOperation, PredicateSelector};
use crate::util::MultiValueMap;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::Arc;

/// The access a standalone caller of [`stub_matches`]/[`predicate_matches`] runs a predicate
/// `inject` under: no imposter door chose one, so every `require()` is refused (issue #1611).
fn refused_script_files() -> &'static Arc<crate::imposter::ScriptFileAccess> {
    static REFUSED: std::sync::LazyLock<Arc<crate::imposter::ScriptFileAccess>> =
        std::sync::LazyLock::new(|| Arc::new(crate::imposter::ScriptFileAccess::Refused));
    &REFUSED
}

/// A borrowed, multi-value view over request headers, so the one predicate engine can serve both
/// the imposter hot path (one value per header name) and the intercept proxy (every value of a
/// repeated header, issue #994) without either paying for the other's shape.
pub trait RequestHeaders {
    /// Every header name paired with all of its values, in receipt order. A name never yields an
    /// empty slice — it wouldn't be iterated at all if it carried no values.
    fn entries(&self) -> impl Iterator<Item = (&str, &[String])>;
    /// Number of distinct header names.
    fn len(&self) -> usize;
    /// Whether there are no header names at all.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The imposter hot path's shape: one value per header name. `std::slice::from_ref` turns the
/// single `&String` into a one-element slice with no allocation, so this adapter costs nothing on
/// the per-request hot path.
impl<SH: BuildHasher> RequestHeaders for HashMap<String, String, SH> {
    fn entries(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.iter()
            .map(|(k, v)| (k.as_str(), std::slice::from_ref(v)))
    }

    fn len(&self) -> usize {
        self.len()
    }
}

/// The intercept proxy's shape (issue #994): every value a repeated header carried, in the order
/// the client sent them.
impl<SH: BuildHasher> RequestHeaders for HashMap<String, Vec<String>, SH> {
    fn entries(&self) -> impl Iterator<Item = (&str, &[String])> {
        // Skipping empty vectors is what makes the trait's "never an empty slice" invariant hold
        // by construction rather than by convention. This map is public API, so a caller can hand
        // over `name -> []`; without this filter that name would read as *present* to `exists`,
        // never-matching to `equals`, and absent to `inject` — three answers to one input.
        self.iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    fn len(&self) -> usize {
        // Counted the same way `entries()` yields, or `deepEquals` — which compares this against
        // the expected object's name count — would see a name that no other operator can observe.
        self.values().filter(|v| !v.is_empty()).count()
    }
}

/// A parsed query or form (issue #1596): the same shape as the header map above, kept in request
/// order.
impl<SH: BuildHasher> RequestHeaders for indexmap::IndexMap<String, Vec<String>, SH> {
    fn entries(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    fn len(&self) -> usize {
        self.values().filter(|v| !v.is_empty()).count()
    }
}

/// Check if a stub matches a request based on its predicates.
///
/// `Err` propagates a predicate-`inject` failure (issue #440: object-build failure or the
/// script itself throwing) — Mountebank fails the request loud on this rather than treating the
/// predicate as non-matching, so callers must surface the error rather than swallow it into
/// `false`. Every other predicate op is infallible.
#[allow(clippy::too_many_arguments)]
pub fn stub_matches<H>(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &H,
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
    H: RequestHeaders,
{
    // Parse the body and query once for standalone callers; the request hot path parses each once
    // per request (before the stub scan) and calls `stub_matches_inner` directly (issues #290, #480).
    let body_json = LazyJson::new(body);
    let query_map = parse_query(query);
    let form_fast = form.map(one_value_per_key);
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
        refused_script_files(),
        &body_json,
        None,
        Some(&query_map),
    )
}

/// Body of [`stub_matches`] taking the once-parsed request body so every predicate reuses one
/// parse rather than re-parsing the body per predicate and per stub (issue #290).
///
/// See [`stub_matches`] for the `Err` contract (issue #440).
#[allow(clippy::too_many_arguments)]
pub(crate) fn stub_matches_inner<H>(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &H,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — always sourced from `Imposter::parse_form_data` (issue #704) on the
    // request hot path; unlike `headers`, this can't be generic (a bare `None` form argument, the
    // common non-form-predicate case, gives type inference nothing to pin the hasher to).
    form: Option<&MultiValueMap>,
    imposter_port: u16,
    // What a predicate `inject` may `require()` (issue #1611).
    script_files: &Arc<crate::imposter::ScriptFileAccess>,
    body_json: &LazyJson<'_>,
    // The once-per-request lazily-parsed XML DOM (issue #711): `Some` on the request hot path
    // (`find_matching_stub_with_client`/`_linear`), which constructs one `LazyXmlDom` before the
    // stub loop and shares it across every stub's XPath predicates, so the body is parsed at most
    // once per request no matter how many XPath predicates evaluate it. `None` for standalone
    // callers (the `stub_matches` wrapper, tests) — they fall back to a per-call parse.
    xml_dom: Option<&LazyXmlDom<'_>>,
    // Always sourced from `parse_query`/`parse_query_string` (issue #704); same reasoning as `form`.
    query_map: Option<&MultiValueMap>,
) -> anyhow::Result<bool>
where
    H: RequestHeaders,
{
    Ok(stub_matches_explained(
        predicates,
        method,
        path,
        query,
        headers,
        body,
        request_from,
        client_ip,
        form,
        imposter_port,
        script_files,
        body_json,
        xml_dom,
        query_map,
    )?
    .is_none())
}

/// [`stub_matches_inner`]'s scan, reporting WHICH predicate rejected the stub: `Some(i)` is the
/// position in `predicates` of the first predicate the request failed, `None` means every
/// predicate passed (including the empty list, which matches everything).
///
/// This is the boolean scan, not a second one beside it — `stub_matches_inner` is defined in terms
/// of it — so the answer can never disagree with the match itself. The index, not a field path, is
/// the reportable datum: a predicate's field is a key inside its operation's map and `and`/`or`
/// nest further, so there is no single path to name; the journal's consumer renders the predicate
/// by indexing the stub config.
///
/// Predicates are implicitly AND-ed and the scan short-circuits, so predicates after the reported
/// index were never evaluated.
///
/// See [`stub_matches`] for the `Err` contract (issue #440).
#[allow(clippy::too_many_arguments)]
pub(crate) fn stub_matches_explained<H>(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &H,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&MultiValueMap>,
    imposter_port: u16,
    // What a predicate `inject` may `require()` (issue #1611).
    script_files: &Arc<crate::imposter::ScriptFileAccess>,
    body_json: &LazyJson<'_>,
    xml_dom: Option<&LazyXmlDom<'_>>,
    query_map: Option<&MultiValueMap>,
) -> anyhow::Result<Option<usize>>
where
    H: RequestHeaders,
{
    for (index, predicate) in predicates.iter().enumerate() {
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
            script_files,
            body_json,
            xml_dom,
            query_map,
        )? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Parse query string for predicate matching, URL-decoding both keys and values. A repeated key
/// keeps every value (issue #1553).
pub fn parse_query(query: Option<&str>) -> MultiValueMap {
    query.map_or_else(MultiValueMap::default, parse_query_string)
}

/// A caller's one-value-per-key form, as the engine's multi-valued shape: the public
/// [`stub_matches`]/[`predicate_matches`] take a form whose keys each carry one value.
fn one_value_per_key(form: &HashMap<String, String>) -> MultiValueMap {
    form.iter()
        .map(|(k, v)| (k.clone(), vec![v.clone()]))
        .collect()
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
pub fn predicate_matches<H>(
    predicate: &Predicate,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &H,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // See `stub_matches` for why this stays the plain std-hasher map.
    form: Option<&HashMap<String, String>>,
    imposter_port: u16,
) -> anyhow::Result<bool>
where
    H: RequestHeaders,
{
    // Standalone callers parse the body and query here; the request hot path parses each once per
    // request and calls `predicate_matches_inner` directly with the shared parses (issues #290, #480).
    let body_json = LazyJson::new(body);
    let query_map = parse_query(query);
    let form_fast = form.map(one_value_per_key);
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
        refused_script_files(),
        &body_json,
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
// `imposter_port` and `script_files` are consumed by the `inject` predicate evaluator, which is
// `javascript`-gated (issue #599); without that feature the arm doesn't use them, so they are only
// threaded through the recursive And/Or descent — a false positive for this build only.
#[cfg_attr(not(feature = "javascript"), allow(clippy::only_used_in_recursion))]
pub(crate) fn predicate_matches_inner<H>(
    predicate: &Predicate,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &H,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `stub_matches_inner`.
    form: Option<&MultiValueMap>,
    imposter_port: u16,
    // What a predicate `inject` may `require()` (issue #1611).
    script_files: &Arc<crate::imposter::ScriptFileAccess>,
    body_json: &LazyJson<'_>,
    // The once-per-request lazily-parsed XML DOM (issue #711) — see `stub_matches_inner`.
    xml_dom: Option<&LazyXmlDom<'_>>,
    // Concretely `FastMap` — see `stub_matches_inner`.
    query_map: Option<&MultiValueMap>,
) -> anyhow::Result<bool>
where
    H: RequestHeaders,
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
    // preserves the previous fall-through-to-unchanged behavior. Case-insensitive unless the
    // predicate is case sensitive, as Mountebank compiles it (`gi`, issue #1329).
    let except_regex = except_pattern.and_then(|pattern| cached_regex(pattern, !case_sensitive));

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
    // Mountebank's `matches` normalizes the request as case sensitive, so its `except` never
    // ignores case whatever the predicate says.
    let matches_except_regex = except_pattern.and_then(|pattern| cached_regex(pattern, false));
    let apply_matches_except = as_except_fn(|value| match &matches_except_regex {
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
    let query_map: &MultiValueMap = match query_map {
        Some(q) => q,
        None => {
            parsed_query = parse_query(query);
            &parsed_query
        }
    };
    let body_str = body.unwrap_or("");

    // What a jsonpath/xpath selector picks out of the body (issue #1257): one value, or several, in
    // which case the predicate holds when any of them satisfies it. Reuses the once-per-request body
    // parse and DOM when the caller threaded them (issues #290, #711) and the compiled-selector
    // caches. A selector that does not compile is refused at load (#1220), so the empty-string
    // fallback only covers a body that is not JSON/XML, which Mountebank also reads as `""`.
    let selection = match &predicate.parameters.selector {
        Some(PredicateSelector::JsonPath { selector }) => Some(
            match body_json.get() {
                Some(json) => jsonpath_selection(json, selector),
                None => serde_json::from_str::<serde_json::Value>(body_str)
                    .ok()
                    .and_then(|json| jsonpath_selection(&json, selector)),
            }
            .unwrap_or(Selection::One(String::new())),
        ),
        Some(PredicateSelector::XPath {
            selector,
            namespaces,
        }) => {
            let own_dom;
            let dom = match xml_dom {
                Some(dom) => dom,
                None => {
                    own_dom = LazyXmlDom::new(body_str);
                    &own_dom
                }
            };
            // Mountebank lowercases the body, the selector and the `ns` URIs for a case-insensitive
            // predicate other than `matches` (issue #1478, see `fold_xml_names`). The prefixes are
            // lowercased too, which Mountebank does not do, so an upper-case prefix still binds
            // once the selector is folded.
            // A selector that fails when it runs fails the request (issue #1585), as Mountebank's
            // `xpathSelect` does; it never reads as the empty string. A body that is not XML never
            // runs the selector, so it reads as `""`, as in Mountebank.
            let selection = if !case_sensitive && folds_xpath_names(&predicate.operation) {
                let namespaces = namespaces.as_ref().map(|ns| {
                    ns.iter()
                        .map(|(prefix, uri)| {
                            (prefix.to_ascii_lowercase(), uri.to_ascii_lowercase())
                        })
                        .collect::<HashMap<_, _>>()
                });
                dom.folded_document()
                    .map(|document| {
                        xpath_selection(
                            &document,
                            &fold_xpath_selector(selector),
                            namespaces.as_ref(),
                        )
                    })
                    .transpose()?
            } else {
                dom.document()
                    .map(|document| xpath_selection(&document, selector, namespaces.as_ref()))
                    .transpose()?
            };
            Some(selection.unwrap_or(Selection::One(String::new())))
        }
        None => None,
    };

    // The once-parsed body corresponds to the compared body only when there is no selector; with a
    // jsonpath/xpath selector the compared body is a selected value, so don't reuse the parse.
    let absent = LazyJson::absent();
    let field_body_json = if predicate.parameters.selector.is_none() {
        body_json
    } else {
        &absent
    };

    // One leaf operator against one candidate body.
    let leaf = |kind: LeafKind, fields: &HashMap<String, serde_json::Value>, body: &str| -> bool {
        match kind {
            LeafKind::Equals | LeafKind::DeepEquals => check_predicate_fields(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                &apply_except,
                str_equals,
                kind == LeafKind::DeepEquals,
                case_sensitive,
                NullLeaf::Identity,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
            ),
            LeafKind::Contains => check_predicate_fields(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                &apply_except,
                |expected, actual| str_contains(actual, expected),
                false,
                case_sensitive,
                NullLeaf::NoText,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
            ),
            LeafKind::StartsWith => check_predicate_fields(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                &apply_except,
                |expected, actual| str_starts_with(actual, expected),
                false,
                case_sensitive,
                NullLeaf::NoText,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
            ),
            LeafKind::EndsWith => check_predicate_fields(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                &apply_except,
                |expected, actual| str_ends_with(actual, expected),
                false,
                case_sensitive,
                NullLeaf::NoText,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
            ),
            LeafKind::Matches => check_predicate_fields_regex(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                &apply_matches_except,
                case_sensitive,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
            ),
            LeafKind::Exists => check_exists_predicate(
                fields,
                method,
                path,
                query_map,
                headers,
                body,
                request_from,
                client_ip,
                form,
                key_case_sensitive,
                field_body_json,
                &apply_except,
            ),
        }
    };
    let evaluate = |kind: LeafKind, fields: &HashMap<String, serde_json::Value>| -> bool {
        match &selection {
            None => leaf(kind, fields, body_str),
            Some(Selection::One(value)) => leaf(kind, fields, value),
            Some(Selection::Many(values)) => holds_for_any_selected(kind, fields, values, &leaf),
        }
    };

    match &predicate.operation {
        PredicateOperation::Equals(fields) => Ok(evaluate(LeafKind::Equals, fields)),
        PredicateOperation::DeepEquals(fields) => Ok(evaluate(LeafKind::DeepEquals, fields)),
        PredicateOperation::Contains(fields) => Ok(evaluate(LeafKind::Contains, fields)),
        PredicateOperation::StartsWith(fields) => Ok(evaluate(LeafKind::StartsWith, fields)),
        PredicateOperation::EndsWith(fields) => Ok(evaluate(LeafKind::EndsWith, fields)),
        PredicateOperation::Matches(fields) => Ok(evaluate(LeafKind::Matches, fields)),
        PredicateOperation::Exists(fields) => Ok(evaluate(LeafKind::Exists, fields)),
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
            script_files,
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
                    script_files,
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
                    script_files,
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
                    // Reuse the once-per-request query map instead of re-parsing (issue #480). A
                    // script sees a repeated key comma-joined (`docs/mountebank/responses.md`).
                    query: crate::util::comma_joined(query_map),
                    // `MountebankRequest.headers` is the fixed single-value scripting boundary
                    // (out of scope for #704/#994), so a repeated header exposes only its first
                    // value to an inject script.
                    headers: headers
                        .entries()
                        .filter_map(|(k, v)| v.first().map(|first| (k.to_string(), first.clone())))
                        .collect(),
                    // `body` is already the classified string from the caller (base64 for a
                    // binary request body, issue #636); this predicate-inject path doesn't thread
                    // the mode flag through separately, so default to `Text` — the script still
                    // sees the correct (base64) string, it just can't ask `isBinary`.
                    body: body.map(|b| b.to_string()),
                    // The matcher sees a body that did not decode as a missing one (issue #1584)
                    // and does not carry the difference here, so this script reads it as `""`.
                    body_undefined: false,
                    mode: None,
                    request_from: request_from.map(str::to_string),
                    ip: client_ip.map(str::to_string),
                    // The matcher's form is parsed for any urlencoded body; a script sees one only
                    // for a non-empty body, as Mountebank builds `request.form`.
                    form: form
                        .filter(|_| body.is_some_and(|b| !b.is_empty()))
                        .map(crate::util::comma_joined),
                    is_dry_run: false,
                };
                execute_predicate_inject(inject_fn, &mb_request, imposter_port, script_files)
            }
            #[cfg(not(feature = "javascript"))]
            {
                tracing::warn!(
                    "inject predicate requires the 'javascript' feature; predicate will not match"
                );
                let _ = (inject_fn, script_files);
                Ok(false)
            }
        }
    }
}

pub(crate) mod arrays;
mod fields;
pub(crate) mod json;
pub(crate) mod regex_cache;
use fields::{check_predicate_fields, check_predicate_fields_regex};
use json::{NullLeaf, check_exists_predicate};
use regex_cache::cached_regex;

/// Whether a case-insensitive XPath predicate with this operator selects from the name-folded
/// document (issue #1478). Mountebank evaluates `matches` case-sensitively, so it selects from the
/// body as written.
pub(crate) fn folds_xpath_names(operation: &PredicateOperation) -> bool {
    matches!(
        operation,
        PredicateOperation::Equals(_)
            | PredicateOperation::DeepEquals(_)
            | PredicateOperation::Contains(_)
            | PredicateOperation::StartsWith(_)
            | PredicateOperation::EndsWith(_)
            | PredicateOperation::Exists(_)
    )
}

/// The operators that compare request fields, as opposed to `not`/`and`/`or`/`inject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeafKind {
    Equals,
    DeepEquals,
    Contains,
    StartsWith,
    EndsWith,
    Matches,
    Exists,
}

/// A leaf predicate whose selector picked several values (issue #1257), as Mountebank's
/// predicates.js evaluates it: a scalar expected `body` holds when any value satisfies it; an array
/// expected `body` needs every expected element to be satisfied by some value; `deepEquals` needs
/// an array holding the same values as a multiset. The rule lives in [`arrays`], shared with repeated
/// query, form and header keys (issue #1553). The other fields do not depend on the selection and
/// are checked with each candidate as usual.
fn holds_for_any_selected(
    kind: LeafKind,
    fields: &HashMap<String, serde_json::Value>,
    values: &[String],
    leaf: &impl Fn(LeafKind, &HashMap<String, serde_json::Value>, &str) -> bool,
) -> bool {
    let Some(expected) = fields.get("body") else {
        // No `body` constraint: the selection is irrelevant, any candidate gives the same answer.
        return leaf(kind, fields, &values[0]);
    };
    let expecting = |element: &serde_json::Value| {
        let mut narrowed = fields.clone();
        narrowed.insert("body".to_string(), element.clone());
        narrowed
    };
    let test = |element: &serde_json::Value, value: &String| {
        // A scalar expectation is tested as written; only an array's elements need narrowing.
        if std::ptr::eq(element, expected) {
            leaf(kind, fields, value)
        } else {
            leaf(kind, &expecting(element), value)
        }
    };
    match kind {
        LeafKind::DeepEquals => arrays::deep_equals_array(expected, values, test),
        _ => arrays::satisfied_by_array(expected, values, test),
    }
}

/// Re-exported from [`crate::util`], its one home (issue #1153), so the long-standing public path
/// `rift_mock_core::imposter::parse_query_string` keeps working.
pub use crate::util::parse_query_string;

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
    // Multi-valued query parameters (issues #83, #1553)
    // A repeated key keeps every value, as Mountebank's `querystring.parse` builds an array:
    // ?key=first&key=second → ["first", "second"]. #83 asked for Mountebank's any-of matching; the
    // comma-join it got (`"first,second"` matched) did not deliver it.
    // =========================================================================

    #[test]
    fn test_parse_query_string_multi_valued_keeps_every_value() {
        let result = parse_query_string("key=first&key=second");

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get("key"),
            Some(&vec!["first".to_string(), "second".to_string()]),
            "a repeated key keeps every value in order"
        );
    }

    fn query_equals(expected: serde_json::Value, query: &str) -> bool {
        let fields: HashMap<String, serde_json::Value> =
            [("query".to_string(), expected)].into_iter().collect();
        let pred = make_predicate(PredicateOperation::Equals(fields));
        predicate_matches(
            &pred,
            "GET",
            "/test",
            Some(query),
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap()
    }

    #[test]
    fn test_equals_query_multi_valued_param() {
        // Inverted by #1553: the comma-joined spelling no longer matches two values, which
        // Mountebank never matched; each value is matched on its own instead.
        assert!(
            !query_equals(json!({"key": "first,second"}), "key=first&key=second"),
            "the comma-joined spelling must not match two separate values"
        );
        assert!(query_equals(
            json!({"key": "first,second"}),
            "key=first,second"
        ));
        assert!(query_equals(
            json!({"key": "first"}),
            "key=first&key=second"
        ));
        assert!(query_equals(
            json!({"key": "second"}),
            "key=first&key=second"
        ));
        assert!(query_equals(
            json!({"key": ["second", "first"]}),
            "key=first&key=second"
        ));
    }

    /// Whether the predicate JSON `predicate` matches a GET with `query` and `headers`.
    fn json_predicate_matches(
        predicate: serde_json::Value,
        query: &str,
        headers: &[(&str, &str)],
    ) -> bool {
        let pred: Predicate = serde_json::from_value(predicate).expect("predicate");
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in headers {
            map.entry((*k).to_string())
                .or_default()
                .push((*v).to_string());
        }
        predicate_matches(
            &pred,
            "GET",
            "/t",
            Some(query),
            &map,
            None,
            None,
            None,
            None,
            0,
        )
        .unwrap()
    }

    // Issue #1553: `except` and case folding apply to every value of a repeated key, on both sides.
    #[test]
    fn except_and_case_folding_apply_to_each_value_of_a_repeated_key() {
        let q = "key=first1&key=second2";
        assert!(json_predicate_matches(
            json!({"equals": {"query": {"key": ["FIRST", "Second"]}}, "except": "\\d+"}),
            q,
            &[]
        ));
        assert!(json_predicate_matches(
            json!({"deepEquals": {"query": {"key": ["second", "first"]}}, "except": "\\d+"}),
            q,
            &[]
        ));
        assert!(!json_predicate_matches(
            json!({"equals": {"query": {"key": ["FIRST", "second"]}}, "except": "\\d+", "caseSensitive": true}),
            q,
            &[]
        ));
        assert!(json_predicate_matches(
            json!({"matches": {"query": {"key": ["^f", "^s"]}}}),
            q,
            &[]
        ));
        assert!(!json_predicate_matches(
            json!({"matches": {"query": {"key": ["^f", "^t"]}}}),
            q,
            &[]
        ));
    }

    // Issue #1553: `exists` over a repeated key holds outright for a truthy expectation
    // (`predicates.js` `onlyActualIsArray`), and a falsy one holds when some value is empty.
    #[test]
    fn exists_over_a_repeated_key() {
        assert!(json_predicate_matches(
            json!({"exists": {"query": {"key": true}}}),
            "key=&key=",
            &[]
        ));
        assert!(json_predicate_matches(
            json!({"exists": {"query": {"key": false}}}),
            "key=a&key=",
            &[]
        ));
        assert!(!json_predicate_matches(
            json!({"exists": {"query": {"key": false}}}),
            "key=a&key=b",
            &[]
        ));
    }

    // Issue #1553: the `deepEquals` multiset counts duplicates, and the key set must still agree.
    #[test]
    fn deep_equals_over_a_repeated_key_counts_duplicates_and_keys() {
        let deep = |v: serde_json::Value| json!({"deepEquals": {"query": {"key": v}}});
        assert!(json_predicate_matches(
            deep(json!(["first", "first"])),
            "key=first&key=first",
            &[]
        ));
        assert!(!json_predicate_matches(
            deep(json!(["first", "second"])),
            "key=first&key=first",
            &[]
        ));
        assert!(!json_predicate_matches(
            deep(json!(["first", "second"])),
            "key=first&key=second&other=1",
            &[]
        ));
        // A one-element array is not a key sent once, nor the reverse.
        assert!(!json_predicate_matches(
            deep(json!(["first"])),
            "key=first",
            &[]
        ));
        assert!(!json_predicate_matches(
            deep(json!("first")),
            "key=first&key=first",
            &[]
        ));
    }

    // Issue #1553: a repeated header now takes an array expectation, with the same rule.
    #[test]
    fn an_array_expectation_over_a_repeated_header() {
        let headers = [("X-K", "first"), ("X-K", "second")];
        assert!(json_predicate_matches(
            json!({"equals": {"headers": {"x-k": ["second", "first"]}}}),
            "",
            &headers
        ));
        assert!(json_predicate_matches(
            json!({"deepEquals": {"headers": {"X-K": ["second", "first"]}}}),
            "",
            &headers
        ));
        assert!(!json_predicate_matches(
            json!({"deepEquals": {"headers": {"X-K": "first"}}}),
            "",
            &headers
        ));
        assert!(!json_predicate_matches(
            json!({"equals": {"headers": {"X-K": ["first"]}}}),
            "",
            &[("X-K", "first")]
        ));
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
            Some(&vec![String::new()]),
            "Bare query param 'flag' should be present with empty value"
        );
    }

    #[test]
    fn test_parse_query_string_mixed_bare_and_valued() {
        let result = parse_query_string("a=1&flag&b=2");

        assert_eq!(result.get("a"), Some(&vec!["1".to_string()]));
        assert_eq!(result.get("b"), Some(&vec!["2".to_string()]));
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
            !result,
            "a bare '?flag' is the empty string, which Mountebank's exists reads as absent (#1403)"
        );
    }

    // =========================================================================
    // Mutation-survivor pins (#1382). Every expected value below is one Mountebank 2.9.1
    // agrees with.
    // =========================================================================

    fn fields_of(key: &str, value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        [(key.to_string(), value)].into_iter().collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn holds(
        op: PredicateOperation,
        body: Option<&str>,
        request_from: Option<&str>,
        client_ip: Option<&str>,
        form: Option<&HashMap<String, String>>,
    ) -> bool {
        predicate_matches(
            &make_predicate(op),
            "GET",
            "/test",
            None,
            &empty_headers(),
            body,
            request_from,
            client_ip,
            form,
            0,
        )
        .unwrap()
    }

    fn exists_body(expected: serde_json::Value, body: Option<&str>) -> bool {
        holds(
            PredicateOperation::Exists(fields_of("body", expected)),
            body,
            None,
            None,
            None,
        )
    }

    #[test]
    fn exists_request_from_and_ip_follow_presence() {
        for (key, value) in [("requestFrom", "127.0.0.1:5000"), ("ip", "127.0.0.1")] {
            let on = |expected: bool, v: Option<&str>| {
                let (rf, ip) = if key == "ip" { (None, v) } else { (v, None) };
                holds(
                    PredicateOperation::Exists(fields_of(key, json!(expected))),
                    None,
                    rf,
                    ip,
                    None,
                )
            };
            assert!(on(true, Some(value)), "{key}: exists true, present");
            assert!(!on(true, None), "{key}: exists true, absent");
            assert!(!on(true, Some("")), "{key}: exists true, empty");
            assert!(!on(false, Some(value)), "{key}: exists false, present");
            assert!(on(false, None), "{key}: exists false, absent");
        }
    }

    #[test]
    fn exists_reaches_into_a_nested_json_object() {
        let nested_true = json!({"a": {"b": true}});
        assert!(exists_body(nested_true.clone(), Some(r#"{"a":{"b":1}}"#)));
        assert!(!exists_body(nested_true.clone(), Some(r#"{"a":{"c":1}}"#)));
        assert!(!exists_body(nested_true, Some(r#"{"x":1}"#)));
        let nested_false = json!({"a": {"b": false}});
        assert!(exists_body(nested_false.clone(), Some(r#"{"a":{"c":1}}"#)));
        assert!(!exists_body(nested_false, Some(r#"{"a":{"b":1}}"#)));
    }

    #[test]
    fn exists_with_a_non_boolean_value_means_present() {
        assert!(exists_body(json!("yes"), Some("x")));
        assert!(!exists_body(json!("yes"), None));
        assert!(exists_body(json!({"a": "yes"}), Some(r#"{"a":1}"#)));
        assert!(!exists_body(json!({"a": "yes"}), Some(r#"{"b":1}"#)));
    }

    // =========================================================================
    // `exists` follows Mountebank's rule (#1403): only the empty string is absent
    // (predicates.js `exists`), a missing container fails any object expectation
    // (`predicateSatisfied`: `if (!actual) return false`), and the expectation is read for
    // JavaScript truthiness.
    // =========================================================================

    #[allow(clippy::too_many_arguments)]
    fn exists_on<H: RequestHeaders>(
        field: &str,
        expected: serde_json::Value,
        query: Option<&str>,
        headers: &H,
        body: Option<&str>,
        form: Option<&HashMap<String, String>>,
        key_case_sensitive: Option<bool>,
    ) -> bool {
        let params = PredicateParameters {
            key_case_sensitive,
            ..Default::default()
        };
        predicate_matches(
            &make_predicate_with_params(
                PredicateOperation::Exists(fields_of(field, expected)),
                params,
            ),
            "POST",
            "/test",
            query,
            headers,
            body,
            None,
            None,
            form,
            0,
        )
        .unwrap()
    }

    fn exists_query(expected: serde_json::Value, query: &str) -> bool {
        exists_on(
            "query",
            expected,
            Some(query),
            &empty_headers(),
            None,
            None,
            None,
        )
    }

    fn exists_json(expected: serde_json::Value, body: &str) -> bool {
        exists_on(
            "body",
            expected,
            None,
            &empty_headers(),
            Some(body),
            None,
            None,
        )
    }

    #[test]
    fn exists_reads_an_empty_query_value_as_absent() {
        assert!(!exists_query(json!({"flag": true}), "flag"), "?flag");
        assert!(exists_query(json!({"flag": false}), "flag"), "?flag");
        assert!(!exists_query(json!({"q": true}), "q="), "?q=");
        assert!(exists_query(json!({"q": false}), "q="), "?q=");
        assert!(exists_query(json!({"q": true}), "q=1"), "?q=1");
        assert!(!exists_query(json!({"q": false}), "q=1"), "?q=1");
        assert!(exists_query(json!({"q": false}), "other=1"), "absent");
    }

    #[test]
    fn exists_reads_the_expectation_for_truthiness() {
        for absent in [json!(0), json!(""), json!(null)] {
            assert!(
                !exists_query(json!({"q": absent}), "q=1"),
                "{absent} means absent"
            );
            assert!(
                exists_query(json!({"q": absent}), "other=1"),
                "{absent} means absent"
            );
        }
        for present in [json!(1), json!("yes"), json!([]), json!({})] {
            assert!(
                exists_query(json!({"q": present}), "q=1"),
                "{present} means present"
            );
        }
    }

    #[test]
    fn exists_reads_an_empty_header_as_absent() {
        let empty: HashMap<String, String> = [("X-E".to_string(), String::new())].into();
        let one = |expected| exists_on("headers", expected, None, &empty, None, None, None);
        assert!(!one(json!({"X-E": true})));
        assert!(one(json!({"X-E": false})));

        // A repeated header is an array in Mountebank: a truthy expectation holds outright, a
        // falsy one holds when some value is empty.
        let repeated = |values: &[&str], expected| {
            let headers: HashMap<String, Vec<String>> = [(
                "X-E".to_string(),
                values.iter().map(|v| v.to_string()).collect(),
            )]
            .into();
            exists_on("headers", expected, None, &headers, None, None, None)
        };
        assert!(repeated(&["a", ""], json!({"X-E": true})));
        assert!(repeated(&["a", ""], json!({"X-E": false})));
        assert!(!repeated(&["a", "b"], json!({"X-E": false})));
        // A value replaces an empty first one instead of joining it.
        assert!(!repeated(&["", "v"], json!({"X-E": false})));
        assert!(repeated(&["", "v"], json!({"X-E": true})));
        assert!(repeated(&["", ""], json!({"X-E": false})));
    }

    #[test]
    fn exists_reads_an_empty_form_field_as_absent() {
        let form: HashMap<String, String> = [("a".to_string(), String::new())].into();
        let on = |expected, form: Option<&HashMap<String, String>>, body: &str| {
            exists_on(
                "form",
                expected,
                None,
                &empty_headers(),
                Some(body),
                form,
                None,
            )
        };
        assert!(!on(json!({"a": true}), Some(&form), "a="));
        assert!(on(json!({"a": false}), Some(&form), "a="));
    }

    /// Mountebank has a `form` only for a non-empty urlencoded body; without one, any `form`
    /// expectation fails, `false` included.
    #[test]
    fn exists_on_a_missing_form_fails_whatever_it_expects() {
        let on = |form: Option<&HashMap<String, String>>, body: Option<&str>| {
            exists_on(
                "form",
                json!({"a": false}),
                None,
                &empty_headers(),
                body,
                form,
                None,
            )
        };
        assert!(!on(None, None), "no body");
        assert!(
            !on(Some(&HashMap::new()), Some("")),
            "empty urlencoded body"
        );
        assert!(!on(None, Some(r#"{"a":1}"#)), "not urlencoded");
    }

    #[test]
    fn exists_reads_an_empty_json_leaf_as_absent() {
        assert!(!exists_json(json!({"a": true}), r#"{"a":""}"#));
        assert!(exists_json(json!({"a": false}), r#"{"a":""}"#));
        assert!(!exists_json(json!({"a": {"b": true}}), r#"{"a":{"b":""}}"#));
        assert!(exists_json(json!({"a": {"b": false}}), r#"{"a":{"b":""}}"#));
        for present in ["0", "false", "null", "{}", "[]"] {
            let body = format!(r#"{{"a":{present}}}"#);
            assert!(exists_json(json!({"a": true}), &body), "{body}: present");
            assert!(!exists_json(json!({"a": false}), &body), "{body}: present");
        }
    }

    #[test]
    fn exists_on_an_empty_body_fails_whatever_it_expects() {
        assert!(!exists_json(json!({"a": false}), ""));
        assert!(!exists_json(json!({"a": true}), ""));
        assert!(!exists_on(
            "body",
            json!({"a": false}),
            None,
            &empty_headers(),
            None,
            None,
            None
        ));
    }

    /// A falsy nested parent (`""`, `null`, `0`, `false`, missing) fails its object expectation.
    #[test]
    fn exists_under_a_falsy_parent_fails_whatever_it_expects() {
        for parent in [r#""""#, "null", "0", "false"] {
            let body = format!(r#"{{"a":{parent}}}"#);
            assert!(!exists_json(json!({"a": {"b": false}}), &body), "{body}");
            assert!(!exists_json(json!({"a": {"b": true}}), &body), "{body}");
        }
        assert!(
            !exists_json(json!({"a": {"b": false}}), r#"{"x":1}"#),
            "missing"
        );
    }

    /// A non-JSON body, or one that parses to a scalar, has no keys: everything in it is absent.
    #[test]
    fn exists_on_a_body_without_keys_finds_everything_absent() {
        assert!(exists_json(json!({"a": false}), "plain text"));
        assert!(exists_json(json!({"a": 0}), "plain text"));
        assert!(!exists_json(json!({"a": true}), "plain text"));
        assert!(
            exists_json(json!({"a": false}), "0"),
            "a non-empty string is not falsy"
        );
    }

    #[test]
    fn exists_over_a_json_array_leaf() {
        assert!(exists_json(json!({"a": true}), r#"{"a":[]}"#));
        assert!(exists_json(json!({"a": true}), r#"{"a":[""]}"#));
        assert!(exists_json(json!({"a": false}), r#"{"a":["x",""]}"#));
        assert!(!exists_json(json!({"a": false}), r#"{"a":["x"]}"#));
        assert!(!exists_json(json!({"a": false}), r#"{"a":[]}"#));
    }

    /// Deviation (documented): Mountebank holds a truthy object expectation over an array without
    /// looking at it. Rift holds it when some element satisfies it.
    #[test]
    fn exists_object_over_an_array_needs_some_element_to_satisfy_it() {
        assert!(exists_json(json!({"a": {"b": true}}), r#"{"a":[{"b":1}]}"#));
        assert!(!exists_json(
            json!({"a": {"b": true}}),
            r#"{"a":[{"c":1}]}"#
        ));
        assert!(!exists_json(
            json!({"a": {"b": false}}),
            r#"{"a":[{"b":1}]}"#
        ));
        assert!(exists_json(
            json!({"a": {"b": false}}),
            r#"{"a":[{"b":1},{"c":1}]}"#
        ));
    }

    /// A body that is itself an array: Mountebank tests the expectation against each element.
    #[test]
    fn exists_on_a_top_level_array_body_tests_each_element() {
        assert!(exists_json(json!({"a": true}), r#"[{"b":1},{"a":1}]"#));
        assert!(!exists_json(json!({"a": true}), r#"[{"b":1}]"#));
    }

    /// Mountebank lowercases body keys unless `keyCaseSensitive` is set (`tryJSON`).
    #[test]
    fn exists_compares_body_keys_by_key_case_sensitivity() {
        assert!(exists_json(json!({"A": true}), r#"{"a":1}"#));
        assert!(exists_json(json!({"A": {"B": true}}), r#"{"a":{"b":1}}"#));
        assert!(!exists_on(
            "body",
            json!({"A": true}),
            None,
            &empty_headers(),
            Some(r#"{"a":1}"#),
            None,
            Some(true)
        ));
    }

    // =========================================================================
    // `except` strips before `exists` tests for empty (#1408): Mountebank's `normalize` runs
    // `exceptTransform` for every operator, so a value stripped to "" is absent.
    // =========================================================================

    #[allow(clippy::too_many_arguments)]
    fn exists_except<H: RequestHeaders>(
        field: &str,
        expected: serde_json::Value,
        except: &str,
        case_sensitive: Option<bool>,
        query: Option<&str>,
        headers: &H,
        body: Option<&str>,
        form: Option<&HashMap<String, String>>,
    ) -> bool {
        let params = PredicateParameters {
            except: except.to_string(),
            case_sensitive,
            ..Default::default()
        };
        predicate_matches(
            &make_predicate_with_params(
                PredicateOperation::Exists(fields_of(field, expected)),
                params,
            ),
            "POST",
            "/test",
            query,
            headers,
            body,
            None,
            None,
            form,
            0,
        )
        .unwrap()
    }

    #[test]
    fn except_strips_a_query_value_to_absent() {
        let on = |expected| {
            exists_except(
                "query",
                expected,
                "^x+$",
                None,
                Some("q=xxx"),
                &empty_headers(),
                None,
                None,
            )
        };
        assert!(!on(json!({"q": true})));
        assert!(on(json!({"q": false})));
        let partly = exists_except(
            "query",
            json!({"q": true}),
            "^x+$",
            None,
            Some("q=xxy"),
            &empty_headers(),
            None,
            None,
        );
        assert!(partly, "a value the pattern leaves non-empty is present");
    }

    #[test]
    fn except_strips_a_header_value_to_absent() {
        let headers: HashMap<String, String> = [("X-T".to_string(), "xxx".to_string())].into();
        let on =
            |expected| exists_except("headers", expected, "x+", None, None, &headers, None, None);
        assert!(!on(json!({"X-T": true})));
        assert!(on(json!({"X-T": false})));
    }

    #[test]
    fn except_strips_a_form_field_to_absent() {
        let form: HashMap<String, String> = [("a".to_string(), "xxx".to_string())].into();
        let on = |expected| {
            exists_except(
                "form",
                expected,
                "x+",
                None,
                None,
                &empty_headers(),
                Some("a=xxx"),
                Some(&form),
            )
        };
        assert!(!on(json!({"a": true})));
        assert!(on(json!({"a": false})));
    }

    #[test]
    fn except_strips_a_json_leaf_to_absent() {
        let on = |expected, body: &str| {
            exists_except(
                "body",
                expected,
                "x+",
                None,
                None,
                &empty_headers(),
                Some(body),
                None,
            )
        };
        assert!(!on(json!({"a": true}), r#"{"a":"xxx"}"#));
        assert!(on(json!({"a": false}), r#"{"a":"xxx"}"#));
        assert!(on(json!({"a": true}), r#"{"a":"xy"}"#));
        // A falsy array check strips each string element.
        assert!(on(json!({"a": false}), r#"{"a":["y","xx"]}"#));
        // A parent stripped to "" is a falsy container.
        assert!(!on(json!({"a": {"b": false}}), r#"{"a":"xxx"}"#));
    }

    #[test]
    fn except_strips_a_whole_body_and_the_path() {
        assert!(!exists_except(
            "body",
            json!(true),
            "x+",
            None,
            None,
            &empty_headers(),
            Some("xxx"),
            None
        ));
        assert!(!exists_except(
            "path",
            json!(true),
            ".*",
            None,
            None,
            &empty_headers(),
            None,
            None
        ));
        assert!(
            !exists_except(
                "body",
                json!({"a": false}),
                "x+",
                None,
                None,
                &empty_headers(),
                Some("xxx"),
                None
            ),
            "a body stripped to empty is the falsy container"
        );
    }

    #[test]
    fn except_on_exists_ignores_case_unless_case_sensitive() {
        let on = |case_sensitive| {
            exists_except(
                "query",
                json!({"q": true}),
                "x+",
                case_sensitive,
                Some("q=XXX"),
                &empty_headers(),
                None,
                None,
            )
        };
        assert!(!on(None), "stripped by default");
        assert!(on(Some(true)), "kept under caseSensitive");
    }

    #[test]
    fn equals_and_matches_compare_request_from_and_ip() {
        let ip = Some("127.0.0.1");
        let rf = Some("127.0.0.1:5000");
        let equals = |key: &str, v: &str| {
            holds(
                PredicateOperation::Equals(fields_of(key, json!(v))),
                None,
                rf,
                ip,
                None,
            )
        };
        let matches = |key: &str, v: &str| {
            holds(
                PredicateOperation::Matches(fields_of(key, json!(v))),
                None,
                rf,
                ip,
                None,
            )
        };
        assert!(equals("ip", "127.0.0.1"));
        assert!(!equals("ip", "10.0.0.1"));
        assert!(equals("requestFrom", "127.0.0.1:5000"));
        assert!(!equals("requestFrom", "10.0.0.1:1"));
        assert!(matches("requestFrom", r"^127\.0\.0\.1:\d+$"));
        assert!(!matches("requestFrom", r"^10\."));
        assert!(matches("ip", r"^127\.0\.0\.1$"));
        assert!(!matches("ip", r"^10\."));
    }

    #[test]
    fn deep_equals_on_form_requires_the_same_field_set() {
        let form: HashMap<String, String> = [("a", "1"), ("b", "2")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let deep = |expected: serde_json::Value| {
            holds(
                PredicateOperation::DeepEquals(fields_of("form", expected)),
                Some("a=1&b=2"),
                None,
                None,
                Some(&form),
            )
        };
        assert!(!deep(json!({"a": "1"})));
        assert!(deep(json!({"a": "1", "b": "2"})));
    }

    #[test]
    fn request_headers_len_and_is_empty_count_only_names_with_values() {
        let multi: HashMap<String, Vec<String>> = [
            ("a".to_string(), vec!["1".to_string()]),
            ("b".to_string(), vec!["2".to_string()]),
        ]
        .into_iter()
        .collect();
        assert_eq!(RequestHeaders::len(&multi), 2);
        assert!(!RequestHeaders::is_empty(&multi));

        let valueless: HashMap<String, Vec<String>> =
            [("a".to_string(), Vec::new())].into_iter().collect();
        assert_eq!(RequestHeaders::len(&valueless), 0);
        assert!(RequestHeaders::is_empty(&valueless));

        let single_empty: HashMap<String, String> = HashMap::new();
        assert!(RequestHeaders::is_empty(&single_empty));
        let single: HashMap<String, String> =
            [("a".to_string(), "1".to_string())].into_iter().collect();
        assert!(!RequestHeaders::is_empty(&single));
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

    // The `matches` key comparison under keyCaseSensitive: true is exact (#1382; Rift honours the
    // setting on its own, a documented deviation from Mountebank, #1367).
    #[test]
    fn matches_with_key_case_sensitive_finds_only_the_exact_key() {
        let fields: HashMap<String, serde_json::Value> =
            [("headers".to_string(), json!({"X-Key": "^v"}))]
                .into_iter()
                .collect();
        let params = PredicateParameters {
            key_case_sensitive: Some(true),
            ..Default::default()
        };
        let pred = make_predicate_with_params(PredicateOperation::Matches(fields), params);
        let matches_with = |name: &str| {
            let headers: HashMap<String, String> = [(name.to_string(), "value".to_string())].into();
            predicate_matches(
                &pred, "GET", "/test", None, &headers, None, None, None, None, 0,
            )
            .unwrap()
        };
        assert!(matches_with("X-Key"), "the exact key is found");
        assert!(!matches_with("x-key"), "another spelling is not");
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

        // After fix #87, the shared request-header collector (issue #1025) produces Title-Case keys
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

        // except removes "/api" from both sides, as in Mountebank (issue #1329): "/users" on each,
        // so the predicate holds. Rift used to strip only the actual value and miss.
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
        assert_eq!(result.get("key"), Some(&vec!["hello world".to_string()]));
        assert_eq!(result.get("name"), Some(&vec!["café".to_string()]));
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
            Some(&vec!["%FF".to_string()]),
            "an undecodable value must pass through raw, not become empty"
        );

        let key = parse_query_string("%FF=v");
        assert_eq!(
            key.get("%FF"),
            Some(&vec!["v".to_string()]),
            "an undecodable key must pass through raw, not collapse to an empty key"
        );

        // The collision the fix exists to prevent: two *different* undecodable keys both blanked
        // to "" used to comma-join their unrelated values into a single bogus entry.
        let collision = parse_query_string("%FF=a&%FE=b");
        assert_eq!(collision.get("%FF"), Some(&vec!["a".to_string()]));
        assert_eq!(collision.get("%FE"), Some(&vec!["b".to_string()]));
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

    // Fix #105: except is now applied to leaf values inside the JSON comparators (`Satisfy`),
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

    // Fix #100 made this a miss; Mountebank compares a top-level expected array by position, so a
    // longer actual whose prefix matches is a hit (issue #1579, verified against 2.9.5).
    #[test]
    fn test_equals_body_array_matches_a_longer_actual_by_prefix() {
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
            result,
            "equals [1, 2] matches [1, 2, 3] by position, as in Mountebank"
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

    // Issue #1364: a recording Mountebank made (the selected value typed, `1`) loads and replays in
    // Rift, which records the string form (`"1"`) itself.
    #[test]
    fn a_mountebank_typed_jsonpath_recording_matches() {
        for recorded in [json!(1), json!("1")] {
            let pred: crate::imposter::Predicate = serde_json::from_value(json!({
                "deepEquals": { "body": recorded.clone() },
                "jsonpath": { "selector": "$.id" }
            }))
            .expect("the recorded predicate decodes");
            for (body, expected) in [(r#"{"id":1,"name":"a"}"#, true), (r#"{"id":2}"#, false)] {
                let matched = predicate_matches(
                    &pred,
                    "POST",
                    "/items",
                    None,
                    &empty_headers(),
                    Some(body),
                    None,
                    None,
                    None,
                    0,
                )
                .expect("evaluates");
                assert_eq!(matched, expected, "{recorded} against {body}");
            }
        }
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
        let headers: HashMap<String, String> = HashMap::new();
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
                refused_script_files(),
                &LazyJson::absent(),
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
                refused_script_files(),
                &LazyJson::absent(),
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
                refused_script_files(),
                &LazyJson::absent(),
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
            refused_script_files(),
            &LazyJson::absent(),
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
            refused_script_files(),
            &LazyJson::absent(),
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

    // =========================================================================
    // stub_matches_explained — the same scan, reporting WHICH predicate rejected
    // =========================================================================

    /// Run `stub_matches_explained` over `predicates` for `GET /api/users`, headerless.
    fn explain(predicates: &[Predicate]) -> Option<usize> {
        stub_matches_explained(
            predicates,
            "GET",
            "/api/users",
            None,
            &empty_headers(),
            None,
            None,
            None,
            None,
            0,
            refused_script_files(),
            &LazyJson::absent(),
            None,
            None,
        )
        .expect("no inject predicate, so this cannot fail")
    }

    fn path_equals(path: &str) -> Predicate {
        make_predicate(PredicateOperation::Equals(
            [("path".to_string(), json!(path))].into_iter().collect(),
        ))
    }

    // An empty predicate list matches everything, so there is no failing predicate to name.
    #[test]
    fn explained_reports_no_failure_for_an_empty_predicate_list() {
        assert_eq!(explain(&[]), None);
    }

    // The reported index is the FIRST failure's position, not merely "some failure": with two
    // failing predicates behind one passing one, the middle index must come back.
    #[test]
    fn explained_reports_the_first_failing_predicate_index() {
        let predicates = vec![
            path_equals("/api/users"),
            path_equals("/nope"),
            path_equals("/also-nope"),
        ];
        assert_eq!(explain(&predicates), Some(1));
        // Reordering moves the reported index with the failure — it is positional, not incidental.
        let predicates = vec![path_equals("/nope"), path_equals("/api/users")];
        assert_eq!(explain(&predicates), Some(0));
        // All passing ⇒ no failure.
        assert_eq!(explain(&[path_equals("/api/users")]), None);
    }

    // The explained scan must be the boolean scan: `stub_matches_inner` is defined in terms of it,
    // so any divergence would silently change which stub matches.
    #[test]
    fn explained_agrees_with_stub_matches_inner() {
        let fixtures: Vec<Vec<Predicate>> = vec![
            vec![],
            vec![path_equals("/api/users")],
            vec![path_equals("/nope")],
            vec![path_equals("/api/users"), path_equals("/nope")],
            vec![make_predicate(PredicateOperation::Equals(
                [("method".to_string(), json!("GET"))].into_iter().collect(),
            ))],
        ];
        for predicates in &fixtures {
            let boolean = stub_matches_inner(
                predicates,
                "GET",
                "/api/users",
                None,
                &empty_headers(),
                None,
                None,
                None,
                None,
                0,
                refused_script_files(),
                &LazyJson::absent(),
                None,
                None,
            )
            .expect("no inject predicate");
            assert_eq!(
                boolean,
                explain(predicates).is_none(),
                "explained and boolean scans disagreed on {predicates:?}"
            );
        }
    }

    // =========================================================================
    // Issue #1059: `matches` and the four string operators fold case differently
    // on non-ASCII, deliberately.
    // =========================================================================

    /// Pins **both halves** of a divergence that looks like a bug and is not, so that a future
    /// "fix" has to argue with the record rather than silently flip one of them.
    ///
    /// `equals`/`deepEquals`/`contains`/`startsWith`/`endsWith` fold ASCII only. That is not a
    /// shortcut: the stub index prunes on a prefix/substring relation and folds with
    /// `to_ascii_lowercase` to stay exactly equivalent to the evaluator — see `core::stub_index::fold`,
    /// whose doc comment is the record. Unicode folding is length-changing and context-sensitive —
    /// `startsWith "/ΟΣ"` against `/ΟΣΑ` folds to a final sigma — so adopting it would make the index
    /// prune stubs that still match. #480 is the other half of the reason: the ASCII fold compares
    /// without allocating per request.
    ///
    /// `matches` keeps the regex engine's Unicode folding (`predicates::regex_cache::compile`), which
    /// is also what Mountebank does for it — note Mountebank folds Unicode for the string operators
    /// too, so the divergence is Rift's, taken knowingly. Narrowing `matches` would mean
    /// `unicode(false)`, which changes `\w`, `\d`, `\s`, `\b` and `.` as well.
    #[test]
    fn matches_folds_non_ascii_case_but_equals_does_not() {
        let equals_pred = make_predicate(PredicateOperation::Equals(
            [("path".to_string(), json!("/josé"))].into_iter().collect(),
        ));
        let matches_pred = make_predicate(PredicateOperation::Matches(
            [("path".to_string(), json!("^/josé$"))]
                .into_iter()
                .collect(),
        ));

        let eval = |pred: &Predicate| {
            predicate_matches(
                pred,
                "GET",
                "/JOSÉ",
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

        assert!(
            !eval(&equals_pred),
            "`equals` folds ASCII only, so É and é are compared exactly"
        );
        assert!(
            eval(&matches_pred),
            "`matches` folds per Unicode, so É and é are the same letter"
        );
    }

    #[test]
    fn case_sensitive_true_makes_both_operators_agree_again() {
        // Each operator is probed with the case difference it *would* otherwise fold, and each
        // negative is paired with the baseline positive. Otherwise the assertions pass vacuously:
        // probing `equals` with `É`/`é` asserts a negative that already holds when `caseSensitive`
        // is ignored entirely, since the ASCII fold never folded `É` in the first place.
        let sensitive = PredicateParameters {
            case_sensitive: Some(true),
            ..Default::default()
        };

        let eval = |pred: &Predicate, path: &str| {
            predicate_matches(
                pred,
                "GET",
                path,
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

        let equals_fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/users"))]
                .into_iter()
                .collect();
        assert!(
            eval(
                &make_predicate(PredicateOperation::Equals(equals_fields.clone())),
                "/USERS"
            ),
            "baseline: `equals` folds ASCII case by default"
        );
        assert!(
            !eval(
                &make_predicate_with_params(
                    PredicateOperation::Equals(equals_fields),
                    sensitive.clone()
                ),
                "/USERS"
            ),
            "caseSensitive: true stops `equals` folding ASCII case"
        );

        let matches_fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("^/josé$"))]
                .into_iter()
                .collect();
        assert!(
            eval(
                &make_predicate(PredicateOperation::Matches(matches_fields.clone())),
                "/JOSÉ"
            ),
            "baseline: `matches` folds non-ASCII case by default"
        );
        assert!(
            !eval(
                &make_predicate_with_params(PredicateOperation::Matches(matches_fields), sensitive),
                "/JOSÉ"
            ),
            "caseSensitive: true stops `matches` folding non-ASCII case"
        );
    }

    #[test]
    fn ascii_case_folding_is_unaffected_by_the_divergence() {
        // The divergence is non-ASCII only; both operators must still agree on ASCII, which is
        // what the overwhelming majority of stubs actually rely on.
        let equals_pred = make_predicate(PredicateOperation::Equals(
            [("path".to_string(), json!("/users"))]
                .into_iter()
                .collect(),
        ));
        let matches_pred = make_predicate(PredicateOperation::Matches(
            [("path".to_string(), json!("^/users$"))]
                .into_iter()
                .collect(),
        ));

        for pred in [&equals_pred, &matches_pred] {
            assert!(
                predicate_matches(
                    pred,
                    "GET",
                    "/USERS",
                    None,
                    &empty_headers(),
                    None,
                    None,
                    None,
                    None,
                    0,
                )
                .unwrap(),
                "both operators fold ASCII case by default"
            );
        }
    }

    // Issue #1257: a selector that picks several values matches when any of them satisfies the
    // operator, as Mountebank's predicates.js does (`expectedMatchesAtLeastOneValueInActualArray`);
    // `deepEquals` compares the sorted list. Expected values come from Mountebank's own
    // `predicates.evaluate` run against the same bodies.
    mod any_selected_value {
        use super::*;

        const TWO_ZS: &str = r#"{"x":{"y":[{"z":"first"},{"z":"second"}]}}"#;

        fn holds(predicate: serde_json::Value, body: &str) -> bool {
            let predicate: Predicate = serde_json::from_value(predicate).expect("predicate json");
            predicate_matches(
                &predicate,
                "POST",
                "/x",
                None,
                &empty_headers(),
                Some(body),
                None,
                None,
                None,
                0,
            )
            .expect("evaluates")
        }

        fn jp(op: &str, expected: serde_json::Value, selector: &str) -> serde_json::Value {
            json!({ op: { "body": expected }, "jsonpath": { "selector": selector } })
        }

        #[test]
        fn scalar_operators_match_any_selected_value() {
            let sel = "$.x.y[*].z";
            assert!(holds(jp("equals", json!("second"), sel), TWO_ZS));
            assert!(holds(jp("equals", json!("first"), sel), TWO_ZS));
            assert!(!holds(jp("equals", json!("third"), sel), TWO_ZS));
            assert!(holds(jp("contains", json!("cond"), sel), TWO_ZS));
            assert!(holds(jp("startsWith", json!("sec"), sel), TWO_ZS));
            assert!(holds(jp("endsWith", json!("ond"), sel), TWO_ZS));
            assert!(holds(jp("matches", json!("^sec"), sel), TWO_ZS));
            assert!(!holds(jp("matches", json!("^thi"), sel), TWO_ZS));
            // caseSensitive is still honoured per value.
            assert!(holds(jp("equals", json!("SECOND"), sel), TWO_ZS));
            let strict = json!({ "equals": { "body": "SECOND" }, "caseSensitive": true,
                                 "jsonpath": { "selector": sel } });
            assert!(!holds(strict, TWO_ZS));
        }

        #[test]
        fn an_expected_array_needs_every_value_to_be_selected() {
            let sel = "$.x.y[*].z";
            assert!(holds(jp("equals", json!(["second", "first"]), sel), TWO_ZS));
            assert!(!holds(jp("equals", json!(["first", "third"]), sel), TWO_ZS));
        }

        #[test]
        fn exists_looks_at_every_selected_value() {
            let sel = "$.x.y[*].z";
            assert!(holds(jp("exists", json!(true), sel), TWO_ZS));
            assert!(!holds(jp("exists", json!(false), sel), TWO_ZS));
            assert!(holds(jp("exists", json!(false), "$.x.nope"), TWO_ZS));
            assert!(!holds(jp("exists", json!(true), "$.x.nope"), TWO_ZS));
        }

        #[test]
        fn deep_equals_compares_the_sorted_selection() {
            let sel = "$.x.y[*].z";
            assert!(!holds(jp("deepEquals", json!("first"), sel), TWO_ZS));
            assert!(holds(
                jp("deepEquals", json!(["second", "first"]), sel),
                TWO_ZS
            ));
            assert!(!holds(jp("deepEquals", json!(["first"]), sel), TWO_ZS));
            assert!(!holds(
                jp("deepEquals", json!(["first", "second", "third"]), sel),
                TWO_ZS
            ));
            // A single selected scalar still compares as a scalar.
            assert!(holds(
                jp("deepEquals", json!("first"), "$.x.y[0].z"),
                TWO_ZS
            ));
        }

        #[test]
        fn a_single_selected_array_is_matched_element_wise() {
            let body = r#"{"tags":["a","important"],"one":["x"]}"#;
            assert!(holds(jp("equals", json!("important"), "$.tags"), body));
            assert!(holds(jp("contains", json!("port"), "$.tags"), body));
            assert!(!holds(jp("equals", json!("b"), "$.tags"), body));
            assert!(holds(jp("equals", json!("x"), "$.one"), body));
            assert!(holds(jp("deepEquals", json!(["x"]), "$.one"), body));
            assert!(!holds(jp("deepEquals", json!("x"), "$.one"), body));
            assert!(holds(
                jp("deepEquals", json!(["important", "a"]), "$.tags"),
                body
            ));
        }

        #[test]
        fn other_fields_still_have_to_hold() {
            let pred = |method: &str| {
                json!({ "equals": { "method": method, "body": "second" },
                        "jsonpath": { "selector": "$.x.y[*].z" } })
            };
            assert!(holds(pred("POST"), TWO_ZS));
            assert!(!holds(pred("GET"), TWO_ZS), "the request is a POST");
            // No `body` constraint: the selection does not matter.
            let method_only = json!({ "equals": { "method": "POST" },
                                      "jsonpath": { "selector": "$.x.y[*].z" } });
            assert!(holds(method_only, TWO_ZS));
        }

        #[test]
        fn not_and_or_wrap_the_any_value_result() {
            let eq = |v: &str| jp("equals", json!(v), "$.x.y[*].z");
            assert!(!holds(json!({ "not": eq("first") }), TWO_ZS));
            assert!(holds(json!({ "not": eq("third") }), TWO_ZS));
            assert!(holds(json!({ "or": [eq("third"), eq("second")] }), TWO_ZS));
            assert!(holds(json!({ "and": [eq("first"), eq("second")] }), TWO_ZS));
            assert!(!holds(json!({ "and": [eq("first"), eq("third")] }), TWO_ZS));
        }

        #[test]
        fn deep_equals_sorts_after_except_is_removed() {
            let pred = json!({ "deepEquals": { "body": ["a", "b"] }, "except": "^zz-",
                               "jsonpath": { "selector": "$.v[*]" } });
            assert!(holds(pred, r#"{"v":["zz-a","b"]}"#));
        }

        #[test]
        fn except_applies_to_each_selected_value() {
            let pred = json!({ "equals": { "body": "a" }, "except": "^x-",
                               "jsonpath": { "selector": "$.v[*]" } });
            assert!(holds(pred, r#"{"v":["b","x-a"]}"#));
        }

        #[test]
        fn exists_with_mixed_empty_values() {
            let body = r#"{"v":[{"s":"a"},{"s":""}]}"#;
            assert!(holds(jp("exists", json!(false), "$.v[*].s"), body));
            assert!(holds(jp("exists", json!(true), "$.v[*].s"), body));
            let all_empty = r#"{"v":[{"s":""},{"s":""}]}"#;
            assert!(!holds(jp("exists", json!(true), "$.v[*].s"), all_empty));
        }

        #[test]
        fn deep_equals_sorts_case_insensitively_unless_case_sensitive() {
            let body = r#"{"v":["A","b"]}"#;
            assert!(holds(jp("deepEquals", json!(["B", "a"]), "$.v[*]"), body));
            let strict = json!({ "deepEquals": { "body": ["B", "a"] }, "caseSensitive": true,
                                 "jsonpath": { "selector": "$.v[*]" } });
            assert!(!holds(strict, body));
        }

        #[test]
        fn numbers_compare_by_their_text() {
            let body = r#"{"items":[{"id":1},{"id":2}]}"#;
            assert!(holds(jp("equals", json!(2), "$.items[*].id"), body));
            assert!(holds(
                jp("deepEquals", json!([2, 1]), "$.items[*].id"),
                body
            ));
            assert!(!holds(jp("equals", json!(3), "$.items[*].id"), body));
        }

        #[test]
        fn xpath_matches_any_selected_node() {
            let xml = "<users><user><name>ann</name></user><user><name>bob</name></user></users>";
            let xp = |op: &str, expected: serde_json::Value| json!({ op: { "body": expected }, "xpath": { "selector": "//user/name" } });
            assert!(holds(xp("equals", json!("bob")), xml));
            assert!(holds(xp("equals", json!("ann")), xml));
            assert!(!holds(xp("equals", json!("cat")), xml));
            assert!(holds(xp("deepEquals", json!(["bob", "ann"])), xml));
        }
    }

    /// Issue #1329: Mountebank strips `except` from the expected value as well as the actual one
    /// (`predicates.js` `create`/`deepEquals` normalize both with the predicate's config), except
    /// under `matches`, whose expected value is a pattern. A predicate holding a raw value plus
    /// `except` — what a predicate generator records — then matches every request that differs
    /// only in the stripped part.
    mod except_on_both_sides {
        use super::*;

        fn holds_on(predicate: serde_json::Value, path: &str, body: &str) -> bool {
            let predicate: Predicate = serde_json::from_value(predicate).expect("predicate json");
            predicate_matches(
                &predicate,
                "GET",
                path,
                Some("id=7"),
                &empty_headers(),
                Some(body),
                None,
                None,
                None,
                0,
            )
            .expect("evaluates")
        }

        #[test]
        fn deep_equals_strips_the_expected_value() {
            let pred = json!({ "deepEquals": { "path": "/users/123" }, "except": "\\d+" });
            assert!(holds_on(pred.clone(), "/users/456", ""));
            assert!(holds_on(pred, "/users/123", ""));
        }

        #[test]
        fn equals_strips_the_expected_value() {
            let pred = json!({ "equals": { "path": "/users/123" }, "except": "\\d+" });
            assert!(holds_on(pred.clone(), "/users/456", ""));
            assert!(!holds_on(pred, "/orders/456", ""));
        }

        #[test]
        fn object_leaves_are_stripped_on_both_sides() {
            let pred = json!({ "deepEquals": { "query": { "id": "3" } }, "except": "\\d+" });
            assert!(holds_on(pred, "/", ""));
            let body = json!({ "equals": { "body": { "ref": "a-1" } }, "except": "-\\d+$" });
            assert!(holds_on(body, "/", r#"{"ref": "a-2"}"#));
        }

        #[test]
        fn matches_keeps_the_pattern_whole() {
            // Stripping `d` from the pattern would turn its `\d$` into `\$`, a literal dollar.
            let pred = json!({ "matches": { "path": "^/users/\\d$" }, "except": "d" });
            assert!(holds_on(pred, "/users/1", ""));
            let stripping = json!({ "matches": { "path": "^/a$" }, "except": "a" });
            assert!(
                !holds_on(stripping, "/a", ""),
                "the actual value is still stripped"
            );
        }

        /// Under `matches` Mountebank compiles `except` case sensitive, whatever the predicate says.
        #[test]
        fn except_under_matches_stays_case_sensitive() {
            let pred = json!({ "matches": { "path": "^/a$" }, "except": "X" });
            assert!(!holds_on(pred, "/ax", ""));
        }

        /// Mountebank compiles `except` with `gi` unless the predicate is case sensitive.
        #[test]
        fn except_follows_case_sensitivity() {
            let insensitive = json!({ "equals": { "path": "/a" }, "except": "X" });
            assert!(holds_on(insensitive, "/ax", ""));
            let sensitive =
                json!({ "equals": { "path": "/a" }, "except": "X", "caseSensitive": true });
            assert!(!holds_on(sensitive, "/ax", ""));
        }
    }
}
