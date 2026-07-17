//! JSON-shaped predicate helpers: value stringification, recursive `exists` checks,
//! and recursive JSON comparison used by the `equals`/`deepEquals`/`matches` operators.

use crate::util::FastMap;
use std::collections::HashMap;
use std::hash::BuildHasher;

/// Convert a JSON value to its string representation for predicate comparison.
/// Strings are unwrapped (no quotes), other primitives use their natural representation.
fn json_value_to_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        _ => val.to_string(),
    }
}

/// Recursively check field existence within a JSON object.
/// When the exists value is an object, parse the actual string as JSON
/// and check each field's existence recursively (Mountebank compatible).
fn check_exists_json_recursive(expected: &serde_json::Value, actual_str: &str) -> bool {
    match expected {
        serde_json::Value::Bool(should_exist) => {
            let exists = !actual_str.is_empty();
            exists == *should_exist
        }
        serde_json::Value::Object(expected_obj) => {
            let actual_json: serde_json::Value = match serde_json::from_str(actual_str) {
                Ok(v) => v,
                Err(_) => {
                    // If we can't parse as JSON, check if any field expects non-existence
                    return expected_obj
                        .values()
                        .all(|v| v == &serde_json::Value::Bool(false));
                }
            };

            for (key, expected_val) in expected_obj {
                match expected_val {
                    serde_json::Value::Bool(should_exist) => {
                        let exists = actual_json.get(key).is_some();
                        if exists != *should_exist {
                            return false;
                        }
                    }
                    serde_json::Value::Object(_) => {
                        // Recurse into nested object
                        let nested_str = match actual_json.get(key) {
                            Some(v) => json_value_to_string(v),
                            None => return false,
                        };
                        if !check_exists_json_recursive(expected_val, &nested_str) {
                            return false;
                        }
                    }
                    _ => {
                        // Non-boolean, non-object values are treated as true (field should exist)
                        if actual_json.get(key).is_none() {
                            return false;
                        }
                    }
                }
            }
            true
        }
        _ => {
            // Non-boolean, non-object exists values: treat as "should exist" = true
            !actual_str.is_empty()
        }
    }
}

/// Check exists predicate - verifies field presence or absence
/// Supports: method, path, body, query, headers, form, requestFrom, ip
/// When a field's value is an object (not a boolean), parse the actual value as JSON
/// and recursively check field existence within it (Mountebank compatible).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_exists_predicate<SH>(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    // Concretely `FastMap` — always sourced from `parse_query`/`parse_query_string` (issue #704).
    query: &FastMap<String, String>,
    headers: &HashMap<String, String, SH>,
    body: &str,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `check_predicate_fields`.
    form: Option<&FastMap<String, String>>,
    key_case_sensitive: bool,
) -> bool
where
    SH: BuildHasher,
{
    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    // Check method exists (always present in HTTP requests)
    if let Some(expected) = obj.get("method") {
        let should_exist = expected.as_bool().unwrap_or(true);
        let exists = !method.is_empty();
        if exists != should_exist {
            return false;
        }
    }

    // Check path exists (always present in HTTP requests)
    if let Some(expected) = obj.get("path") {
        let should_exist = expected.as_bool().unwrap_or(true);
        let exists = !path.is_empty();
        if exists != should_exist {
            return false;
        }
    }

    // Check requestFrom exists
    if let Some(expected) = obj.get("requestFrom") {
        let should_exist = expected.as_bool().unwrap_or(true);
        let exists = request_from.is_some_and(|v| !v.is_empty());
        if exists != should_exist {
            return false;
        }
    }

    // Check ip exists
    if let Some(expected) = obj.get("ip") {
        let should_exist = expected.as_bool().unwrap_or(true);
        let exists = client_ip.is_some_and(|v| !v.is_empty());
        if exists != should_exist {
            return false;
        }
    }

    // Check body exists - supports both boolean and object values
    if let Some(expected) = obj.get("body")
        && !check_exists_json_recursive(expected, body)
    {
        return false;
    }

    // Check query parameters exist
    if let Some(expected_query) = obj.get("query").and_then(|v| v.as_object()) {
        for (key, should_exist_val) in expected_query {
            let should_exist = should_exist_val.as_bool().unwrap_or(true);
            let exists = query.iter().any(|(k, _)| key_matches(key, k));
            if exists != should_exist {
                return false;
            }
        }
    }

    // Check headers exist
    if let Some(expected_headers) = obj.get("headers").and_then(|v| v.as_object()) {
        for (key, should_exist_val) in expected_headers {
            let should_exist = should_exist_val.as_bool().unwrap_or(true);
            let exists = headers.iter().any(|(k, _)| key_matches(key, k));
            if exists != should_exist {
                return false;
            }
        }
    }

    // Check form fields exist
    if let Some(expected_form) = obj.get("form").and_then(|v| v.as_object()) {
        let actual_form = form.cloned().unwrap_or_default();
        for (key, should_exist_val) in expected_form {
            let should_exist = should_exist_val.as_bool().unwrap_or(true);
            let exists = actual_form.iter().any(|(k, _)| key_matches(key, k));
            if exists != should_exist {
                return false;
            }
        }
    }

    true
}

/// Recursively apply a comparison function when the expected value is a JSON object.
/// Parses the actual string as JSON and compares each field recursively.
/// For leaf values, converts both to strings and applies the comparison function.
/// When `deep_equals` is true, also verifies no extra keys exist in actual objects
/// and arrays are compared structurally (same length, element-wise).
/// `key_case_sensitive` controls whether JSON object key lookups are case-sensitive.
/// `apply_except` is applied to leaf values (not raw JSON strings) to avoid breaking
/// JSON structure before parsing.
///
/// `pre_parsed` is the request body already parsed into a `serde_json::Value` once per request
/// (issue #290). When `Some`, it is used at this top level instead of re-parsing `actual_str` —
/// it is exactly `serde_json::from_str(actual_str)` on the same bytes, so the result is identical.
/// Recursive calls over nested (re-stringified) values always pass `None`.
pub(crate) fn compare_json_recursive<F>(
    expected: &serde_json::Value,
    actual_str: &str,
    compare: &F,
    deep_equals: bool,
    key_case_sensitive: bool,
    apply_except: &dyn for<'a> Fn(&'a str) -> std::borrow::Cow<'a, str>,
    pre_parsed: Option<&serde_json::Value>,
) -> bool
where
    F: Fn(&str, &str) -> bool,
{
    match expected {
        serde_json::Value::Object(expected_obj) => {
            let parsed_owned;
            let actual_json: &serde_json::Value = match pre_parsed {
                Some(v) => v,
                None => match serde_json::from_str(actual_str) {
                    Ok(v) => {
                        parsed_owned = v;
                        &parsed_owned
                    }
                    Err(_) => return false,
                },
            };

            let Some(actual_obj) = actual_json.as_object() else {
                return false;
            };

            // For deepEquals, actual must have exactly the same keys
            if deep_equals && expected_obj.len() != actual_obj.len() {
                return false;
            }

            for (key, expected_val) in expected_obj {
                let actual_val = if key_case_sensitive {
                    actual_obj.get(key)
                } else {
                    actual_obj
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(key))
                        .map(|(_, v)| v)
                };

                let Some(actual_val) = actual_val else {
                    return false;
                };

                let actual_val_str = json_value_to_string(actual_val);
                if !compare_json_recursive(
                    expected_val,
                    &actual_val_str,
                    compare,
                    deep_equals,
                    key_case_sensitive,
                    apply_except,
                    None,
                ) {
                    return false;
                }
            }
            true
        }
        serde_json::Value::Array(expected_arr) => {
            let parsed_owned;
            let actual_json: &serde_json::Value = match pre_parsed {
                Some(v) => v,
                None => match serde_json::from_str(actual_str) {
                    Ok(v) => {
                        parsed_owned = v;
                        &parsed_owned
                    }
                    Err(_) => return false,
                },
            };

            let Some(actual_arr) = actual_json.as_array() else {
                return false;
            };

            if expected_arr.len() != actual_arr.len() {
                return false;
            }

            for (expected_elem, actual_elem) in expected_arr.iter().zip(actual_arr.iter()) {
                let actual_elem_str = json_value_to_string(actual_elem);
                if !compare_json_recursive(
                    expected_elem,
                    &actual_elem_str,
                    compare,
                    deep_equals,
                    key_case_sensitive,
                    apply_except,
                    None,
                ) {
                    return false;
                }
            }
            true
        }
        _ => {
            let expected_str = json_value_to_string(expected);
            let actual_str = apply_except(actual_str);
            compare(&expected_str, &actual_str)
        }
    }
}

/// Whether a string could itself be JSON — trimmed, it starts with `{` or `[`.
///
/// Such a leaf is the one case a purely structural hash cannot handle: `compare_json_recursive`
/// re-stringifies actual values and re-parses when the expected side is a container, so a string
/// leaf `"{\"b\":1}"` is equivalent to the object `{"b":1}` (in **both** directions). See
/// [`structural_hash`].
fn is_jsonish_string(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

/// A finalizing bit-mixer (MurmurHash3 fmix64) so structurally-derived intermediate values don't
/// collide on low bits.
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// Order-sensitive combine of two sub-hashes (arrays, and key↔value within an object entry).
fn combine(a: u64, b: u64) -> u64 {
    mix64(a ^ mix64(b).rotate_left(31))
}

/// Hash a string leaf under the comparator's default fold.
///
/// `FixedState` (not `RandomState`): this `u64` is *stored* in the body-index map and computed at
/// two different times (index build vs request), so it must be stable within the process — the same
/// reason `decision_cache::hash_body` uses it.
fn hash_folded(s: &str) -> u64 {
    // Default (non-`caseSensitive`) leaf comparison is `eq_ignore_ascii_case`, so folding with
    // `to_ascii_lowercase` before hashing makes case-equal leaves hash-equal — exactly and only
    // ASCII, matching the evaluator (Unicode folding would over-collapse; see `stub_index::fold`).
    foldhash::fast::FixedState::default().hash_one(s.to_ascii_lowercase())
}

/// Container type tags, keeping `{}`/`[]`/leaves in distinct hash spaces. Leaves carry **no** tag,
/// so the comparator's type coercion (`1`≡`"1"`, `true`≡`"true"`, `null`≡`""`) is preserved.
const OBJ_TAG: u64 = 0x9e37_79b9_7f4a_7c15;
const ARR_TAG: u64 = 0xc2b2_ae3d_27d4_eb4f;

/// A structural hash of a JSON value under the **deepEquals default-mode** comparator's equivalence
/// relation — the one the body-hash index (#708) probes with. It is derived from
/// [`compare_json_recursive`]'s own shape, NOT from `serde_json::Value::eq`:
///
/// - **Leaves** hash `fold(json_value_to_string(v))`, so cross-type coercions the comparator treats
///   as equal collapse to one bucket: `1`≡`"1"`, `true`≡`"true"`, `null`≡`""`, and ASCII-case-equal
///   strings. Distinct stringifications stay distinct (`1.0`≢`1`).
/// - **Objects** combine commutatively (key order is irrelevant — deepEquals matches keys
///   case-insensitively by default) over `combine(hash(fold(key)), hash(value))`, plus a length tag
///   (deepEquals is length-exact).
/// - **Arrays** combine order-sensitively, plus a length tag.
///
/// Returns `None` when the value contains a JSON-ish string leaf (see [`is_jsonish_string`]): no
/// structural hash can reconcile a string with the container it is equivalent to, so the caller
/// treats `None` as "not indexable" at build time and "do not prune this dimension" at request time
/// — both of which only ever *widen* the candidate set, so soundness is preserved.
///
/// Collisions are harmless: the index over-approximates and Stage-2 (`compare_json_recursive`)
/// rejects false candidates. The one real bug class is *under*-approximation — a value equal under
/// the comparator hashing differently — which is what the `structural_hash` property test guards.
pub(crate) fn structural_hash(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => {
            let mut acc: u64 = 0;
            for (k, v) in map {
                let entry = combine(hash_folded(k), structural_hash(v)?);
                // Commutative (wrapping-add) so object key order does not affect the hash, but each
                // entry is `mix64`'d first so `{"a":"b"}` and `{"b":"a"}` do not cancel.
                acc = acc.wrapping_add(mix64(entry));
            }
            Some(combine(OBJ_TAG, combine(map.len() as u64, acc)))
        }
        serde_json::Value::Array(arr) => {
            let mut h = ARR_TAG;
            for v in arr {
                h = combine(h, structural_hash(v)?);
            }
            Some(combine(h, arr.len() as u64))
        }
        serde_json::Value::String(s) if is_jsonish_string(s) => None,
        // String (non-JSON-ish), Number, Bool, Null: the comparator compares the *stringified* leaf,
        // so hash that same normalization. `json_value_to_string(String)` is the raw string, so this
        // arm treats a plain string and the scalar that stringifies to it identically.
        _ => Some(hash_folded(&json_value_to_string(value))),
    }
}

#[cfg(test)]
mod structural_hash_tests {
    use super::*;
    use serde_json::{Value, json};

    /// deepEquals in default mode (the exact relation the body index indexes): case-insensitive
    /// leaves, case-insensitive keys, length-exact, no `except`, no selector.
    fn deep_equals_default(expected: &Value, actual: &Value) -> bool {
        compare_json_recursive(
            expected,
            &json_value_to_string(actual),
            &|e: &str, a: &str| e.eq_ignore_ascii_case(a),
            true,  // deep_equals
            false, // key_case_sensitive (defaults to caseSensitive = false)
            &|s: &str| std::borrow::Cow::Borrowed(s),
            Some(actual),
        )
    }

    /// The soundness contract: if two values are deepEquals-equal, either they hash the same, or one
    /// of them is un-indexable (a JSON-ish string leaf → `None`, which makes the dimension not prune).
    /// Anything else would be an under-approximation — a matching stub silently dropped.
    fn assert_sound(e: &Value, a: &Value) {
        if deep_equals_default(e, a) {
            let (he, ha) = (structural_hash(e), structural_hash(a));
            assert!(
                he.is_none() || ha.is_none() || he == ha,
                "under-approximation: deep_equals holds but hashes differ\n e={e}\n a={a}\n he={he:?} ha={ha:?}"
            );
        }
    }

    #[test]
    fn concrete_cross_type_equivalences_hash_equal() {
        // Each pair deep-equals in default mode and must therefore hash-agree.
        for (e, a) in [
            (json!({"n": 1}), json!({"n": "1"})), // number ≡ its string
            (json!({"b": true}), json!({"b": "true"})), // bool ≡ its string
            (json!({"z": null}), json!({"z": ""})), // null ≡ empty string
            (json!({"s": "ABC"}), json!({"s": "abc"})), // ASCII case fold
            (json!({"a": 1, "b": 2}), json!({"b": 2, "a": 1})), // object key order
            (json!({"Key": 1}), json!({"key": 1})), // key case (keyCaseSensitive default false)
            (json!([1, "2", null]), json!(["1", 2, ""])), // nested array coercions
            (json!({"o": {"x": 1}}), json!({"o": {"x": "1"}})), // nested object
        ] {
            assert!(
                deep_equals_default(&e, &a),
                "precondition: {e} deep-equals {a}"
            );
            assert_eq!(
                structural_hash(&e),
                structural_hash(&a),
                "{e} and {a} deep-equal but hash differently"
            );
            assert!(structural_hash(&e).is_some());
        }
    }

    #[test]
    fn distinct_numbers_stay_distinct() {
        // 1.0 and 1 stringify differently ("1.0" vs "1"), do NOT deep-equal, and must not collide.
        assert!(!deep_equals_default(&json!({"n": 1.0}), &json!({"n": 1})));
        assert_ne!(
            structural_hash(&json!({"n": 1.0})),
            structural_hash(&json!({"n": 1}))
        );
    }

    #[test]
    fn json_in_string_leaf_bails_both_directions() {
        // A string leaf that could be JSON is equivalent to the container it encodes, in both
        // directions — structural_hash must return None so the caller widens rather than prunes.
        assert_eq!(structural_hash(&json!({"a": "{\"b\":1}"})), None);
        assert_eq!(structural_hash(&json!({"a": " [1,2]"})), None); // leading space, still JSON-ish
        assert_eq!(structural_hash(&json!(["{}"])), None);
        // And the equivalence it protects really holds under the comparator:
        assert!(deep_equals_default(
            &json!({"a": {"b": 1}}),
            &json!({"a": "{\"b\":1}"})
        ));
        // A plain string that merely contains a brace later is not JSON-ish → still indexable.
        assert!(structural_hash(&json!({"a": "x{y"})).is_some());
    }

    #[test]
    fn empty_container_types_are_distinct() {
        assert_ne!(structural_hash(&json!({})), structural_hash(&json!([])));
        assert_ne!(structural_hash(&json!({})), structural_hash(&json!("")));
    }

    #[test]
    fn array_order_is_significant() {
        // deepEquals compares arrays element-wise (order-sensitive), so the hash must be too —
        // `[1,2]` and `[2,1]` do not deep-equal and should not collide.
        assert!(!deep_equals_default(&json!([1, 2]), &json!([2, 1])));
        assert_ne!(
            structural_hash(&json!([1, 2])),
            structural_hash(&json!([2, 1]))
        );
    }

    // A tiny deterministic LCG — a seeded, dependency-free "proptest-style" generator (the design
    // sign-off asked for ~10k seeded cases). Determinism matters so a failure is reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
        fn coin(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }
    }

    fn gen_value(rng: &mut Lcg, depth: u32) -> Value {
        // Leaf bias grows with depth so trees terminate.
        let leaf = depth == 0 || rng.below(100) < (40 + u64::from(depth) * 25);
        if leaf {
            match rng.below(7) {
                0 => Value::Null,
                1 => Value::Bool(rng.coin()),
                2 => json!(rng.below(4) as i64), // small ints so "n" ≡ "\"n\"" pairs recur
                3 => json!(rng.below(3) as f64 + 0.5), // non-integer floats
                4 => Value::String(["a", "B", "aB", ""][rng.below(4) as usize].to_string()),
                5 => Value::String(rng.below(4).to_string()), // digit strings ≡ numbers
                _ => Value::String("{jsonish".to_string()),   // exercises the bail path
            }
        } else if rng.coin() {
            let n = rng.below(3);
            let mut m = serde_json::Map::new();
            for i in 0..n {
                let key = ["k", "K", "other"][i as usize % 3].to_string();
                m.insert(key, gen_value(rng, depth - 1));
            }
            Value::Object(m)
        } else {
            let n = rng.below(3);
            Value::Array((0..n).map(|_| gen_value(rng, depth - 1)).collect())
        }
    }

    /// Produce a value that deep-equals `v` in default mode (coerce scalars across types, flip ASCII
    /// case, permute object keys) so the property test actually exercises equality, not just the
    /// vacuous `false` branch.
    fn preserving_variant(rng: &mut Lcg, v: &Value) -> Value {
        match v {
            Value::Null => {
                if rng.coin() {
                    Value::String(String::new())
                } else {
                    Value::Null
                }
            }
            Value::Bool(b) => {
                if rng.coin() {
                    Value::String(b.to_string())
                } else {
                    Value::Bool(*b)
                }
            }
            Value::Number(n) => {
                if rng.coin() {
                    Value::String(n.to_string())
                } else {
                    Value::Number(n.clone())
                }
            }
            Value::String(s) => {
                // Case-flip is fold-preserving; leave JSON-ish strings untouched (both sides bail).
                if is_jsonish_string(s) {
                    Value::String(s.clone())
                } else if rng.coin() {
                    Value::String(s.to_uppercase())
                } else {
                    Value::String(s.to_lowercase())
                }
            }
            Value::Array(a) => Value::Array(a.iter().map(|e| preserving_variant(rng, e)).collect()),
            Value::Object(m) => {
                // Rebuild in a different insertion order, flipping key case (default keyCaseSensitive
                // is false). serde_json::Map preserves insertion order, so reversing exercises the
                // commutative object combine.
                let mut entries: Vec<(String, Value)> = m
                    .iter()
                    .map(|(k, val)| {
                        let key = if rng.coin() {
                            k.to_uppercase()
                        } else {
                            k.clone()
                        };
                        (key, preserving_variant(rng, val))
                    })
                    .collect();
                entries.reverse();
                Value::Object(entries.into_iter().collect())
            }
        }
    }

    #[test]
    fn structural_hash_agrees_with_deep_equals_property() {
        let mut rng = Lcg(0x5eed_1234_9e37_79b9);
        let mut deep_equal_cases = 0u32;
        let iterations = 10_000;
        for _ in 0..iterations {
            let a = gen_value(&mut rng, 3);
            // Half the time compare against a comparator-preserving variant (forces frequent
            // equality); half against a freshly generated value (mostly inequality).
            let e = if rng.coin() {
                preserving_variant(&mut rng, &a)
            } else {
                gen_value(&mut rng, 3)
            };
            if deep_equals_default(&e, &a) {
                deep_equal_cases += 1;
            }
            assert_sound(&e, &a);
            // Symmetric: the comparator is not symmetric in general, but the index must be sound
            // whichever side is the indexed (expected) stub.
            assert_sound(&a, &e);
        }
        assert!(
            deep_equal_cases > iterations / 10,
            "generator too weak: only {deep_equal_cases}/{iterations} deep-equal pairs — the \
             soundness assertion would be near-vacuous"
        );
    }
}
