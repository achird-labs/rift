//! Mountebank wire-shape tolerance, shared across crates (issue #936).
//!
//! Mountebank accepts several spellings of the same value — a status code as a number *or* a
//! numeric string, a header as one value *or* an array, a header value as any JSON scalar. Those
//! rules are a property of the wire format, not of any one subsystem, so they live here rather
//! than inside the imposter types: the imposter stub path and the intercept rule schema both
//! parse the same JSON and must agree about what it means. They previously lived in
//! `rift-mock-core` as `pub(crate)`, which is exactly why the intercept path drifted (issue #933
//! for `body`, this issue for `statusCode`/`headers`).

/// Serde for multi-value headers (issue #238). Accepts the Mountebank-style `"k": "v"` *and*
/// `"k": ["v1", "v2"]` on the wire; serializes a single value back as a plain string and multiple
/// values as an array, so existing single-value consumers are unaffected.
///
/// **Invariant on every map this module produces (issue #1039): one entry per case-folded header
/// name.** No two keys `eq_ignore_ascii_case`-match each other; the surviving key is the *first*
/// spelling the deserializer presents, and its values are every case-matching entry's values in
/// that same order. HTTP header names are case-insensitive, so a document that spells one name two
/// ways describes one header — but a `HashMap` keyed by the literal spelling would hold it as two.
/// That split is what the eleven case-insensitive find-first lookups downstream (form parsing,
/// predicate fields, `copy`, the JS/Rhai engines, the verify CLI) resolve nondeterministically, and
/// what makes `deepEquals` on headers compare against the wrong name count. Holding the invariant
/// here — at the single deserializer all five header fields that parse through it share — makes
/// those sites correct by construction rather than by eleven separate case-folding patches. (Two
/// further fields name this module for `serialize_with` only and never reach the deserializer.)
///
/// "First the deserializer presents" is deliberately not "first in the document". Which one you get
/// depends on how the caller reached this function, and **both shapes are live in production**:
///
/// - Deserializing **straight from JSON text or bytes** into the target type streams entries in
///   document order, so the first spelling written wins. (`POST /imposters` and
///   `POST /intercept/rules` take this path, as does `--configfile`'s bare-array form.)
/// - Deserializing from an **already-parsed `serde_json::Value`** walks a `serde_json::Map`, which
///   is a `BTreeMap` unless the `preserve_order` feature is on — it is not enabled in this
///   workspace — so entries arrive sorted by key bytes and the lexicographically smallest spelling
///   wins. (`--configfile`'s `{"imposters": […]}` wrapper and single-object forms both parse to a
///   `Value` first, as do `rift_apply_config` and the scenarios stub path.)
///
/// Note how little those two lists correlate with the user-facing feature: one CLI flag spans both,
/// depending only on the document's outermost punctuation. That is precisely why **nothing outside
/// this module should depend on which spelling wins** — and why the user-facing documentation of
/// this behaviour promises one entry and stops there. What the fix guarantees, and all it needs to
/// guarantee, is that the result is a *function of the document*: the defect was never the choice
/// of spelling but that the choice varied between runs of the same input.
pub mod multi_value_headers {
    use serde::Deserialize;
    use serde::de::{Deserializer, MapAccess, Visitor};
    use serde::ser::{SerializeMap, Serializer};
    use std::collections::HashMap;
    use std::fmt;

    pub fn serialize<S: Serializer>(
        headers: &HashMap<String, Vec<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(headers.len()))?;
        for (key, values) in headers {
            match values.as_slice() {
                [] => continue, // a key with no values would emit no header line; omit it
                [single] => map.serialize_entry(key, single)?,
                many => map.serialize_entry(key, many)?,
            }
        }
        map.end()
    }

    /// A single header value on the wire. Mountebank tolerates non-string scalars — its recorders
    /// routinely emit `"Content-Length": 124` (a JSON number) and `"X-Flag": true` — and coerces
    /// them to their string form. Rift matches that so real recorded imposters load unchanged
    /// (issue #754); previously a numeric/bool value failed both `OneOrMany` variants and rejected
    /// the whole imposter with a 400.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        Str(String),
        Num(serde_json::Number),
        Bool(bool),
    }

    impl Scalar {
        fn into_string(self) -> String {
            match self {
                Scalar::Str(s) => s,
                Scalar::Num(n) => n.to_string(),
                Scalar::Bool(b) => b.to_string(),
            }
        }
    }

    /// Order matters for `#[serde(untagged)]`: a scalar can never match `Many` and an array can
    /// never match `One`, so either order is sound — `One` first keeps the common case first.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Scalar),
        Many(Vec<Scalar>),
    }

    impl OneOrMany {
        /// An empty array yields no values rather than being rejected, which keeps the map
        /// byte-identical to what the pre-#1039 code produced. It is not load-bearing beyond that:
        /// `serialize` omits such an entry, and `RequestHeaders` filters it out of both `entries()`
        /// and `len()`, so a name with no values already reads as absent everywhere downstream.
        fn into_strings(self) -> Vec<String> {
            match self {
                OneOrMany::One(s) => vec![s.into_string()],
                OneOrMany::Many(v) => v.into_iter().map(Scalar::into_string).collect(),
            }
        }
    }

    struct FoldingVisitor;

    impl<'de> Visitor<'de> for FoldingVisitor {
        type Value = HashMap<String, Vec<String>>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map of header names to a scalar or an array of scalars")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
            // Accumulated in a `Vec` rather than a `HashMap` so the fold preserves the order the
            // deserializer presents: which spelling survives and what order the values end up in
            // are then properties of the document, not of a hash iteration order that varies per
            // process. Deserializing straight into a `HashMap` also loses a repeated key silently,
            // since serde's map impl is last-wins.
            //
            // `size_hint` is attacker-influenced in the general case — serde's own map impl clamps
            // it for exactly this reason — so cap the preallocation rather than trusting it. Real
            // header maps are far below the cap, so this never costs a realistic document a
            // reallocation.
            let mut folded: Vec<(String, Vec<String>)> =
                Vec::with_capacity(access.size_hint().unwrap_or(0).min(1024));
            while let Some((name, value)) = access.next_entry::<String, OneOrMany>()? {
                let values = value.into_strings();
                match folded
                    .iter_mut()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                {
                    // `eq_ignore_ascii_case` is exactly the HTTP rule: names differing outside
                    // ASCII are different names and must not be folded together.
                    Some((_, existing)) => existing.extend(values),
                    None => folded.push((name, values)),
                }
            }
            Ok(folded.into_iter().collect())
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<String, Vec<String>>, D::Error> {
        // The linear scan is O(k²) in the number of distinct names. Header maps are small and this
        // runs on config load / the admin API, never on the request path.
        deserializer.deserialize_map(FoldingVisitor)
    }
}

/// Serde for header objects that hold **one** value per name (issue #1050) — `proxy.injectHeaders`
/// and `_rift.fault.error.headers`.
///
/// Deserializing rejects a document that names one header twice, case-insensitively, instead of
/// accepting it and picking a winner. That is deliberately the *opposite* of what
/// [`multi_value_headers`] does, and the difference is not an inconsistency:
///
/// - A multi-value map has a lossless merge — keep both values — so folding costs nothing and
///   #1039 folds.
/// - A single-valued map has none *in general*. Two different values for one name have no correct
///   combination, so a fold would have to pick a winner — and the tie-break cannot even be made
///   deterministic, because which spelling the deserializer presents first depends on whether the
///   document was streamed from text or routed through a `serde_json::Value` first, exactly as
///   documented on [`multi_value_headers`].
///
/// The rule is therefore the simple one — *a single-valued header object names each header once* —
/// and it is applied uniformly. Note that this refuses `{"x": "a", "X": "a"}` too, where a fold
/// *would* be lossless: the case does not rest on the two values conflicting, and the code does not
/// check whether they do. One rule that is always true beats two rules that need the reader to work
/// out which applies.
///
/// Nothing that worked stops working. A document this rejects was previously emitting **two header
/// lines** for that name, ordered by `HashMap` iteration and therefore differently per process:
/// `RequestBuilder::header` and `http::response::Builder::header` both append rather than replace,
/// so both spellings went out on the wire.
///
/// Values stay `String`-only. `multi_value_headers` also coerces JSON numbers and bools (issue
/// #754); widening these two fields to match is a separate compatibility question, not a
/// side effect of this one.
pub mod single_value_headers {
    use serde::de::{Deserializer, Error, MapAccess, Visitor};
    use std::collections::HashMap;
    use std::fmt;

    struct OneEachVisitor;

    impl<'de> Visitor<'de> for OneEachVisitor {
        type Value = HashMap<String, String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map of header names to single string values, each name given once")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
            // A `Vec` so the scan sees the spelling actually written, and so the error can quote
            // it. The order is only used for the diagnostic; the rejection itself does not depend
            // on which of the two the deserializer happened to present first.
            let mut seen: Vec<(String, String)> =
                Vec::with_capacity(access.size_hint().unwrap_or(0).min(1024));
            while let Some((name, value)) = access.next_entry::<String, String>()? {
                if let Some((existing, _)) =
                    seen.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name))
                {
                    return Err(M::Error::custom(format!(
                        "header `{name}` is already given as `{existing}`; a single-valued header \
                         object names each header once"
                    )));
                }
                seen.push((name, value));
            }
            Ok(seen.into_iter().collect())
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<String, String>, D::Error> {
        deserializer.deserialize_map(OneEachVisitor)
    }
}

use serde::Deserialize;

/// Parse a JSON `statusCode` value that may be a number or a (numeric) string.
fn parse_status_code_value<E: serde::de::Error>(value: serde_json::Value) -> Result<u16, E> {
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| E::custom("invalid status code number")),
        serde_json::Value::String(s) => s
            .parse::<u16>()
            .map_err(|_| E::custom(format!("invalid status code string: {s}"))),
        _ => Err(E::custom("statusCode must be a number or string")),
    }
}

/// Deserialize statusCode from either a number or a string
pub fn deserialize_status_code<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    parse_status_code_value(serde_json::Value::deserialize(deserializer)?)
}

/// Deserialize an optional top-level `statusCode` (flat response form, issue #304), reusing the
/// number-or-string parsing. Only invoked when the field is present; a `null` is treated as
/// absent (`None`) so a stray null on a non-flat response stays accepted as before.
pub fn deserialize_optional_status_code<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        value => parse_status_code_value(value).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::collections::HashMap;

    /// Exercises the helpers through serde attributes, the only way they are ever reached — a
    /// direct call would not prove the `#[serde(with = ...)]` wiring still resolves.
    #[derive(Deserialize)]
    struct HeadersIn {
        #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
        headers: HashMap<String, Vec<String>>,
    }

    #[derive(Serialize)]
    struct HeadersOut {
        #[serde(serialize_with = "multi_value_headers::serialize")]
        headers: HashMap<String, Vec<String>>,
    }

    #[derive(Deserialize)]
    struct StatusIn {
        #[serde(deserialize_with = "deserialize_status_code")]
        status: u16,
    }

    #[derive(Deserialize)]
    struct OptionalStatusIn {
        #[serde(default, deserialize_with = "deserialize_optional_status_code")]
        status: Option<u16>,
    }

    #[test]
    fn headers_accept_a_bare_string_or_an_array() {
        let one: HeadersIn = serde_json::from_str(r#"{"headers":{"X-One":"v"}}"#).unwrap();
        assert_eq!(one.headers["X-One"], vec!["v".to_string()]);

        let many: HeadersIn =
            serde_json::from_str(r#"{"headers":{"Set-Cookie":["a","b"]}}"#).unwrap();
        assert_eq!(
            many.headers["Set-Cookie"],
            vec!["a".to_string(), "b".to_string()]
        );
    }

    // Issue #754: recorded imposters carry non-string scalars; they coerce rather than 400.
    #[test]
    fn headers_coerce_numeric_and_bool_scalars() {
        let r: HeadersIn = serde_json::from_str(
            r#"{"headers":{"Content-Length":124,"X-Flag":true,"X-Ratio":1.5,"X-Multi":[200,"x",false]}}"#,
        )
        .expect("numeric/bool header values must be accepted (mb parity)");
        assert_eq!(r.headers["Content-Length"], vec!["124".to_string()]);
        assert_eq!(r.headers["X-Flag"], vec!["true".to_string()]);
        assert_eq!(r.headers["X-Ratio"], vec!["1.5".to_string()]);
        assert_eq!(
            r.headers["X-Multi"],
            vec!["200".to_string(), "x".to_string(), "false".to_string()]
        );
    }

    // Issue #1039: one key per case-folded name. Before this, the deserializer built a
    // `HashMap<String, OneOrMany>`, which both split `content-type` from `Content-Type` into two
    // entries and silently last-wins on a repeated key. Two entries for one header name make every
    // case-insensitive find-first lookup downstream (form parsing, predicates, `copy`, the
    // scripting engines) answer nondeterministically, and make `deepEquals` on headers compare the
    // wrong count.
    #[test]
    fn headers_merge_case_variant_keys_under_the_first_spelling() {
        let r: HeadersIn =
            serde_json::from_str(r#"{"headers":{"content-type":"a","Content-Type":"b"}}"#)
                .expect("case-variant keys are a valid document, not an error");
        assert_eq!(
            r.headers.len(),
            1,
            "two spellings of one name is one header"
        );
        assert_eq!(
            r.headers["content-type"],
            vec!["a".to_string(), "b".to_string()],
            "first spelling survives; values follow document order"
        );
        assert!(
            !r.headers.contains_key("Content-Type"),
            "the later spelling must not survive as a second key"
        );
    }

    #[test]
    fn headers_merge_byte_identical_duplicate_keys() {
        let r: HeadersIn = serde_json::from_str(r#"{"headers":{"X-Dup":"a","X-Dup":"b"}}"#)
            .expect("a repeated key is accepted");
        assert_eq!(
            r.headers["X-Dup"],
            vec!["a".to_string(), "b".to_string()],
            "a repeated key keeps both values instead of serde's silent last-wins"
        );
    }

    #[test]
    fn headers_preserve_a_lone_keys_spelling_byte_exact() {
        // The served header spelling is a contract (`oddly_cased_content_type_not_duplicated`,
        // and the `Content-type` fixture in the SDK corpus whose replay must deep-equal).
        let r: HeadersIn = serde_json::from_str(r#"{"headers":{"Content-type":"x"}}"#).unwrap();
        assert_eq!(r.headers["Content-type"], vec!["x".to_string()]);
        assert!(!r.headers.contains_key("Content-Type"));
        assert!(!r.headers.contains_key("content-type"));

        let shouty: HeadersIn =
            serde_json::from_str(r#"{"headers":{"CONTENT-TYPE":"y"}}"#).unwrap();
        assert_eq!(shouty.headers["CONTENT-TYPE"], vec!["y".to_string()]);

        // The contract is parse *and serve*, so go the whole way back out to the wire — it is the
        // served spelling those guards pin, and asserting only the parsed key leaves that half of
        // the round trip untested.
        let served = serde_json::to_value(HeadersOut { headers: r.headers }).expect("serialize");
        assert_eq!(served["headers"], serde_json::json!({"Content-type": "x"}));
    }

    #[test]
    fn headers_merge_three_way_variants_with_mixed_scalars_and_arrays() {
        let r: HeadersIn =
            serde_json::from_str(r#"{"headers":{"X-A":1,"x-a":[true,"z"],"X-a":2.5}}"#)
                .expect("#754 scalar coercion still applies to every merged entry");
        assert_eq!(r.headers.len(), 1);
        assert_eq!(
            r.headers["X-A"],
            vec![
                "1".to_string(),
                "true".to_string(),
                "z".to_string(),
                "2.5".to_string()
            ]
        );
    }

    #[test]
    fn headers_merge_is_deterministic_across_repeated_parses() {
        // The issue's actual symptom: with a `HashMap` intermediate the surviving key depended on
        // iteration order, so the same stored document answered differently across runs.
        for _ in 0..200 {
            let r: HeadersIn = serde_json::from_str(
                r#"{"headers":{"content-type":"a","Content-Type":"b","CONTENT-TYPE":"c"}}"#,
            )
            .unwrap();
            assert_eq!(r.headers.len(), 1);
            assert_eq!(
                r.headers["content-type"],
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            );
        }
    }

    // The two deserializer inputs present entries in different orders, and both reach production:
    // streaming from text or bytes gives document order, while going through a `serde_json::Value`
    // first walks a `Map` and gives byte order. See the module doc for which callers take which —
    // `--configfile` alone takes both, depending on the document's outermost punctuation, which is
    // why nothing outside this module may depend on the answer.
    //
    // The byte-order half asserts that `preserve_order` is OFF workspace-wide. If a future
    // dependency turns it on, this is the test that should fail — deliberately, and first.
    #[test]
    fn headers_fold_deterministically_from_a_value_too_even_though_the_order_differs() {
        const TEXT: &str = r#"{"headers":{"set-cookie":"a","Set-Cookie":"b"}}"#;

        let from_text: HeadersIn = serde_json::from_str(TEXT).unwrap();
        assert_eq!(
            from_text.headers["set-cookie"],
            vec!["a".to_string(), "b".to_string()],
            "streaming from text keeps document order, so the first spelling written wins"
        );

        let value: serde_json::Value = serde_json::from_str(TEXT).unwrap();
        let from_value: HeadersIn = serde_json::from_value(value).unwrap();
        assert_eq!(from_value.headers.len(), 1, "still one header either way");
        assert_eq!(
            from_value.headers["Set-Cookie"],
            vec!["b".to_string(), "a".to_string()],
            "a `Map` is key-sorted, so `Set-Cookie` (0x53) precedes `set-cookie` (0x73)"
        );
    }

    #[test]
    fn headers_fold_only_ascii_case_and_keep_empty_shapes() {
        let empty: HeadersIn = serde_json::from_str(r#"{"headers":{}}"#).unwrap();
        assert!(empty.headers.is_empty());

        // A key present with an empty array stays present with no values — the pre-#1039 map shape,
        // preserved so the fold changes nothing it does not have to. Downstream it is invisible
        // either way: `serialize` omits it and `RequestHeaders` filters it out of `entries()` and
        // `len()`, so such a name already reads as absent (`header_name_with_no_values_reads_as_absent`).
        let no_values: HeadersIn = serde_json::from_str(r#"{"headers":{"X-None":[]}}"#).unwrap();
        assert_eq!(no_values.headers["X-None"], Vec::<String>::new());

        // `eq_ignore_ascii_case` is exactly the HTTP rule: names differing outside ASCII are
        // different names and must not be folded together.
        let non_ascii: HeadersIn =
            serde_json::from_str(r#"{"headers":{"X-Kä":"1","X-KÄ":"2"}}"#).unwrap();
        assert_eq!(non_ascii.headers.len(), 2);
    }

    #[test]
    fn headers_serialize_single_as_string_many_as_array_and_omit_empty() {
        let out = HeadersOut {
            headers: HashMap::from([
                ("X-One".to_string(), vec!["v".to_string()]),
                (
                    "Set-Cookie".to_string(),
                    vec!["a".to_string(), "b".to_string()],
                ),
                ("X-Empty".to_string(), vec![]),
            ]),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["headers"]["X-One"], serde_json::json!("v"));
        assert_eq!(v["headers"]["Set-Cookie"], serde_json::json!(["a", "b"]));
        assert!(
            v["headers"].get("X-Empty").is_none(),
            "a key with no values emits no header line, so it is omitted"
        );
    }

    /// Exercises `single_value_headers` through a serde attribute, the only way it is reached.
    #[derive(Deserialize, Debug)]
    struct SingleIn {
        #[serde(default, deserialize_with = "single_value_headers::deserialize")]
        headers: HashMap<String, String>,
    }

    // Issue #1050. Before this, both spellings survived as distinct keys and BOTH went out on the
    // wire — `RequestBuilder::header` and `response::Builder::header` append rather than replace —
    // so the peer received two lines for one header, ordered by `HashMap` iteration and therefore
    // differently per process. Refusing costs nothing that was working.
    #[test]
    fn a_name_given_twice_in_different_case_is_rejected() {
        let err = serde_json::from_str::<SingleIn>(
            r#"{"headers":{"content-type":"a","Content-Type":"b"}}"#,
        )
        .expect_err("a single-valued header object names each header once");
        let msg = err.to_string();
        assert!(
            msg.contains("content-type") && msg.contains("Content-Type"),
            "the error must name BOTH spellings so the author can find them: {msg}"
        );
    }

    // The `Value`-mediated path must reject too. Case variants are distinct keys in a
    // `serde_json::Map`, so they both survive to here — unlike a byte-identical duplicate, which
    // the JSON parser has already collapsed by this point (the same two-path split documented on
    // `multi_value_headers`).
    #[test]
    fn a_case_variant_name_is_rejected_through_a_value_too() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"headers":{"x-id":"a","X-Id":"b"}}"#).unwrap();
        assert!(
            serde_json::from_value::<SingleIn>(value).is_err(),
            "routing through a Value must not launder a duplicate past the check"
        );
    }

    #[test]
    fn a_byte_identical_duplicate_is_rejected_on_the_text_path() {
        assert!(
            serde_json::from_str::<SingleIn>(r#"{"headers":{"X-Id":"a","X-Id":"b"}}"#).is_err(),
            "serde's default map visitor would have silently kept the last one"
        );
    }

    #[test]
    fn an_ordinary_single_valued_header_object_is_unchanged() {
        let ok: SingleIn =
            serde_json::from_str(r#"{"headers":{"X-Id":"a","Content-Type":"text/plain"}}"#)
                .expect("every name given once");
        assert_eq!(ok.headers["X-Id"], "a");
        assert_eq!(ok.headers["Content-Type"], "text/plain");
        assert_eq!(ok.headers.len(), 2);

        let empty: SingleIn = serde_json::from_str(r#"{"headers":{}}"#).unwrap();
        assert!(empty.headers.is_empty());

        // Absent is not an error — both fields carry `#[serde(default)]`.
        let missing: SingleIn = serde_json::from_str("{}").unwrap();
        assert!(missing.headers.is_empty());
    }

    // `eq_ignore_ascii_case` is the HTTP rule, as in `multi_value_headers`: names differing outside
    // ASCII are different names, and refusing them would reject a document that is fine.
    #[test]
    fn names_differing_outside_ascii_are_not_treated_as_duplicates() {
        let ok: SingleIn =
            serde_json::from_str(r#"{"headers":{"X-Kä":"1","X-KÄ":"2"}}"#).expect("distinct names");
        assert_eq!(ok.headers.len(), 2);
    }

    #[test]
    fn status_code_accepts_a_number_or_a_numeric_string() {
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":404}"#)
                .unwrap()
                .status,
            404
        );
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":"404"}"#)
                .unwrap()
                .status,
            404
        );
    }

    #[test]
    fn status_code_rejects_junk_rather_than_defaulting() {
        for junk in [
            r#"{"status":"abc"}"#,
            r#"{"status":true}"#,
            // One past `u16::MAX`, in both spellings — the boundary the two parse paths
            // (`str::parse::<u16>` and `u16::try_from(u64)`) have to agree on.
            r#"{"status":65536}"#,
            r#"{"status":"65536"}"#,
            r#"{"status":-1}"#,
            // A non-integer number: `Number::as_u64` yields `None` rather than truncating.
            r#"{"status":200.5}"#,
        ] {
            assert!(
                serde_json::from_str::<StatusIn>(junk).is_err(),
                "{junk} must be an error, not a silent default"
            );
        }

        // …and the boundary itself is still accepted, so the rejections above are landing on the
        // right side of it.
        assert_eq!(
            serde_json::from_str::<StatusIn>(r#"{"status":65535}"#)
                .unwrap()
                .status,
            65535
        );
    }

    #[test]
    fn optional_status_code_treats_null_and_absent_as_none() {
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{}"#)
                .unwrap()
                .status,
            None
        );
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{"status":null}"#)
                .unwrap()
                .status,
            None
        );
        assert_eq!(
            serde_json::from_str::<OptionalStatusIn>(r#"{"status":"201"}"#)
                .unwrap()
                .status,
            Some(201)
        );
    }
}
