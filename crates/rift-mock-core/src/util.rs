//! Shared utility functions used across proxy recording, stub generation,
//! and response building pipelines.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fast, non-cryptographic hash maps for the imposter hot path (issue #704).
///
/// [`FastMap`]/[`FastSet`] swap SipHash for [`foldhash`], which is ~2–4× faster on the short string
/// keys the request path deals in. `foldhash::fast::RandomState` is seeded per-process, so it is not
/// a fixed-seed map an attacker can precompute against, but it is only *minimally* HashDoS-resistant.
///
/// # When to use which
///
/// - **`FastMap`/`FastSet` — request-scoped maps** built from a single request and dropped at
///   response (headers, query, form, per-request template context). Keys come from one request with
///   bounded entry counts (header/query caps), so an attacker cannot accumulate collisions across
///   requests. Also **stub-index maps** rebuilt on stub mutation, whose keys come from operator
///   stub config, not request traffic.
/// - **Keep `std::collections::HashMap` (SipHash)** for any *long-lived* map keyed by *unbounded,
///   request-derived* input, where an attacker could grow a colliding key set over many requests.
///   None exist on the hot path today (the request journal keys by port, not request content).
pub mod fastmap {
    /// A `HashMap` using the fast, non-cryptographic `foldhash` hasher. See the [module docs](self)
    /// for the HashDoS policy governing when this is appropriate.
    pub type FastMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
    /// A `HashSet` using the fast, non-cryptographic `foldhash` hasher. See the [module docs](self).
    pub type FastSet<K> = std::collections::HashSet<K, foldhash::fast::RandomState>;
    /// A [`FastMap`] that iterates in insertion order (issue #1596). The rule: *a map whose
    /// iteration order is visible to a user (scripts, the journal, `MB_REQUEST`) keeps request
    /// order*, because Mountebank's `querystring.parse` does and a script can `JSON.stringify` it.
    /// A plain `FastMap` re-seeds per map, so its order changes from request to request.
    pub type OrderedMap<K, V> = indexmap::IndexMap<K, V, foldhash::fast::RandomState>;
}

pub use fastmap::{FastMap, FastSet, OrderedMap};

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

/// Decode a query/form component as `application/x-www-form-urlencoded`, as Node's
/// `querystring.parse` (and so Mountebank) does: `+` is a space, `%2B` is a literal plus, and
/// text that is not valid percent-encoding passes through unchanged (issue #1607).
///
/// The `+` is replaced *before* percent-decoding, so a decoded `%2B` is never turned into a space.
/// Only query strings and form bodies use this; the path is never decoded.
///
/// An undecodable sequence is Rift's contract from issue #611: it is the client's own text,
/// not an error to erase. Blanking it — the `unwrap_or_default()` this replaced — silently
/// destroys text a predicate matches on, and on a key it collapses distinct parameters into a
/// single `""` entry that then comma-joins unrelated values.
pub fn decode_form_component(value: &str) -> String {
    let spaced = value.replace('+', " ");
    urlencoding::decode(&spaced)
        .map(|decoded| decoded.into_owned())
        // Domain-optional parse (#611): invalid percent-encoding is the client's own text, kept
        // raw. `spaced`, not `value`: Mountebank still turns the `+` beside it into a space.
        .unwrap_or(spaced)
}

/// A parsed query string or urlencoded form: every key with all of its values, in request order
/// (issue #1553). Never holds an empty vector — a key is present only because it was sent.
pub type MultiValueMap = OrderedMap<String, Vec<String>>;

/// Parse a raw query string into a map. The one query parser: predicates, the recorded request,
/// scripts and templates all read a request's query through this, so they cannot disagree about it
/// (issue #1153 — templates used to parse with a different function and saw `?c=a&c=b` as `b`
/// while every other surface saw `a,b`). The urlencoded form is parsed by the same function.
///
/// URL-decodes keys and values with [`decode_form_component`]. A bare parameter with no `=` (`?flag`) is a
/// key with an empty value. A **repeated key keeps every value, in order** (issue #1553), as
/// Mountebank's `querystring.parse` builds an array: `?color=red&color=green` gives
/// `color => ["red", "green"]`. The predicate engine matches each value and scripts see the array
/// ([`values_as_json`], issue #1629); templates render it [`comma_joined`].
#[must_use]
pub fn parse_query_string(query: &str) -> MultiValueMap {
    let mut map = MultiValueMap::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        map.entry(decode_form_component(key))
            .or_default()
            .push(decode_form_component(value));
    }
    map
}

/// The one-string-per-key view of a [`MultiValueMap`]: a repeated key's values comma-joined in
/// order (`["red", "green"]` → `"red,green"`). What `${request.query.x}` templates render, as
/// JavaScript's `String(array)` would (`docs/mountebank/responses.md`). Scripts see the array
/// instead ([`values_as_json`], issue #1629).
#[must_use]
pub fn comma_joined(map: &MultiValueMap) -> OrderedMap<String, String> {
    map.iter().map(|(k, v)| (k.clone(), v.join(","))).collect()
}

/// Whether `key` is an array index in the ECMAScript sense: the canonical decimal form of an
/// integer in `0..=2^32-2`. `"01"`, `"-1"`, `"+1"` and `"4294967295"` are ordinary string keys.
fn is_array_index(key: &str) -> bool {
    key.parse::<u32>()
        .is_ok_and(|n| n != u32::MAX && n.to_string() == key)
}

/// The entries of an insertion-ordered map in the order a JS object would list them (ECMAScript
/// `OrdinaryOwnPropertyKeys`): array-index keys first in ascending numeric order, then every other
/// key in insertion order (issue #1596). What `JSON.stringify` prints for Mountebank's
/// `querystring.parse` result, so a serialized query matches Mountebank byte for byte.
#[must_use]
pub fn js_key_order<V, S>(map: &indexmap::IndexMap<String, V, S>) -> Vec<(&String, &V)> {
    let (mut indexed, other): (Vec<_>, Vec<_>) =
        map.iter().partition(|(key, _)| is_array_index(key));
    // Sorting by the parsed number: every key here parsed as a `u32` in `is_array_index`.
    indexed.sort_by_key(|(key, _)| key.parse::<u32>().unwrap_or(u32::MAX));
    indexed.extend(other);
    indexed
}

/// Serializes an insertion-ordered map as a JSON object in [`js_key_order`] (issue #1596).
pub struct JsKeyOrder<'a, V, S>(pub &'a indexmap::IndexMap<String, V, S>);

impl<V: serde::Serialize, S> serde::Serialize for JsKeyOrder<'_, V, S> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        serializer.collect_map(js_key_order(self.0))
    }
}

/// A field's values as Mountebank shows them (`querystring.parse`, `headersMap`): one value is a
/// string, several are an array. The shape a recorded request, `predicateGenerators` and the
/// verify API's report all use for a query or form key (issue #1553), and every script sees for a
/// query key, form key or header (issue #1629).
#[must_use]
pub fn values_as_json(values: &[String]) -> serde_json::Value {
    match values {
        [one] => serde_json::Value::String(one.clone()),
        many => serde_json::Value::Array(
            many.iter()
                .map(|v| serde_json::Value::String(v.clone()))
                .collect(),
        ),
    }
}

/// A whole [`MultiValueMap`] as a script sees it (issue #1629): each key's [`values_as_json`], in
/// the map's order.
#[must_use]
pub fn map_values_as_json(map: &MultiValueMap) -> OrderedMap<String, serde_json::Value> {
    map.iter()
        .map(|(k, v)| (k.clone(), values_as_json(v)))
        .collect()
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
        FastMap, FastSet, build_response, build_response_with_headers, http2_disabled_from,
        rift_debug_from, strict_behaviors_from,
    };
    use hyper::StatusCode;

    #[test]
    fn fastmap_is_foldhash_backed_and_default_constructible() {
        // The alias must resolve to a foldhash-hashed map, and `Default` must construct it (so
        // migrations can swap `HashMap::new()` for `FastMap::default()` mechanically).
        fn assert_foldhash<K, V>(_: &std::collections::HashMap<K, V, foldhash::fast::RandomState>) {
        }
        let mut map: FastMap<String, i32> = FastMap::default();
        assert_foldhash(&map);
        map.insert("k".to_string(), 1);
        assert_eq!(map.get("k"), Some(&1));

        let mut set: FastSet<String> = FastSet::default();
        set.insert("a".to_string());
        assert!(set.contains("a"));
    }

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

#[cfg(test)]
mod key_order_tests {
    use super::{JsKeyOrder, OrderedMap, js_key_order, parse_query_string};

    // Issue #1596: a repeated key keeps the position of its first occurrence.
    #[test]
    fn parsing_keeps_first_occurrence_order() {
        let params = parse_query_string("b=1&a=2&c=3&a=4");
        let keys: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(keys, ["b", "a", "c"]);
        assert_eq!(params["a"], ["2", "4"]);
    }

    fn json_of(keys: &[&str]) -> String {
        let map: OrderedMap<String, u8> = keys.iter().map(|k| (k.to_string(), 0)).collect();
        serde_json::to_string(&JsKeyOrder(&map)).expect("serializes")
    }

    #[test]
    fn array_index_keys_come_first_ascending_then_insertion_order() {
        assert_eq!(
            json_of(&["b", "10", "2", "a"]),
            r#"{"2":0,"10":0,"b":0,"a":0}"#
        );
    }

    #[test]
    fn only_canonical_array_indexes_sort_first() {
        // "01", "-1", "+1" and "1.5" are strings; 2^32-1 is not an array index, 2^32-2 is.
        assert_eq!(
            json_of(&["01", "-1", "+1", "1.5", "4294967295", "4294967294", "0"]),
            r#"{"0":0,"4294967294":0,"01":0,"-1":0,"+1":0,"1.5":0,"4294967295":0}"#
        );
    }

    #[test]
    fn an_empty_map_has_no_entries() {
        let map: OrderedMap<String, u8> = OrderedMap::default();
        assert!(js_key_order(&map).is_empty());
    }
}

#[cfg(test)]
mod parse_query_string_tests {
    use super::parse_query_string;

    // Ported from the deleted `crate::predicate` module (issue #1153). The #611 and #614 contracts
    // below were pinned against that copy and against the inline copy in `behaviors/request.rs`, but
    // never against this function directly — and this is now the one parser everything uses.

    // Issue #611: an undecodable percent-sequence passes through raw rather than blanking the value a
    // predicate matches on.
    #[test]
    fn an_undecodable_value_passes_through_raw() {
        let params = parse_query_string("k=%FF");
        assert_eq!(one(&params, "k"), Some("%FF"));
    }

    // Issue #614: the key is decoded on the same terms as the value — Mountebank decodes both — and an
    // undecodable key stays raw rather than collapsing to an empty key that would then comma-join
    // unrelated values.
    #[test]
    fn an_encoded_key_is_decoded_and_an_undecodable_one_stays_raw() {
        let params = parse_query_string("first%20name=bob");
        assert_eq!(one(&params, "first name"), Some("bob"));

        let undecodable = parse_query_string("%FF=v");
        assert_eq!(one(&undecodable, "%FF"), Some("v"));
    }

    // Issue #1607: `+` is a space in keys and values (Node's `querystring.parse`); `%2B` stays a
    // plus because the `+` is replaced before percent-decoding; the spaced text survives an
    // undecodable neighbour. `parse_form_data` calls this parser, and its test table in
    // `imposter/core/matching.rs` repeats these rows so the two cannot drift.
    #[test]
    fn a_plus_is_a_space_in_keys_and_values() {
        let cases = [
            ("q=x+y%21", "q", "x y!"),
            ("a+b=1", "a b", "1"),
            ("q=1%2B1", "q", "1+1"),
            ("q=1+1", "q", "1 1"),
            ("q=%2B+%20", "q", "+  "),
            ("r=a+%ZZ", "r", "a %ZZ"),
            ("k=%FF", "k", "%FF"),
        ];
        for (query, key, expected) in cases {
            let params = parse_query_string(query);
            assert_eq!(one(&params, key), Some(expected), "query {query:?}");
        }
    }

    // Issue #614: a bare parameter's key is decoded like a valued one.
    #[test]
    fn a_bare_parameter_key_is_decoded() {
        let params = parse_query_string("first%20name");
        assert_eq!(one(&params, "first name"), Some(""));
    }

    fn one<'a>(params: &'a super::MultiValueMap, key: &str) -> Option<&'a str> {
        match params.get(key).map(Vec::as_slice) {
            Some([value]) => Some(value.as_str()),
            _ => None,
        }
    }

    // Issue #1553: a repeated key keeps every value in request order, as Mountebank's
    // `querystring.parse` builds an array; the comma-joined view is what templates render (#1153).
    #[test]
    fn a_repeated_key_keeps_every_value_in_order() {
        let params = parse_query_string("color=red&color=green&color=blue&one=1");
        assert_eq!(
            params.get("color").map(Vec::as_slice),
            Some(["red", "green", "blue"].map(String::from).as_slice())
        );
        assert_eq!(one(&params, "one"), Some("1"));
        let joined = super::comma_joined(&params);
        assert_eq!(
            joined.get("color").map(String::as_str),
            Some("red,green,blue")
        );
        assert_eq!(joined.get("one").map(String::as_str), Some("1"));
    }

    // One value is a string and several an array, as Mountebank prints a query.
    #[test]
    fn values_render_as_a_string_or_an_array() {
        assert_eq!(super::values_as_json(&["a".into()]), serde_json::json!("a"));
        assert_eq!(
            super::values_as_json(&["a".into(), "b".into()]),
            serde_json::json!(["a", "b"])
        );
    }
}
