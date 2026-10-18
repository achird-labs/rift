//! JSON-shaped predicate helpers: leaf stringification, recursive `exists` checks, and the two
//! JSON comparators Mountebank's `predicates.js` defines — `predicateSatisfied` for
//! `equals`/`contains`/`startsWith`/`endsWith`/`matches` ([`Satisfy`]) and `deepEquals`
//! ([`DeepEquals`]) — plus the structural hash the `deepEquals` body index probes with.

use super::RequestHeaders;
use super::arrays::satisfied_by_array;
use crate::util::MultiValueMap;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;

/// A scalar JSON leaf as JavaScript's `String()` writes it, which is the text every Mountebank
/// operator compares: a string as itself, a number in JS spelling (`1.0` → `1`, `1e2` → `100`,
/// issue #1579), `true`/`false`, and `null` as `"null"`. A container renders as its JSON text,
/// which no comparator reaches (they recurse into containers).
pub(crate) fn leaf_text(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(s) => Cow::Borrowed(s),
        Value::Number(n) => Cow::Owned(rift_types::wire::js_number_string(n)),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::Null => Cow::Borrowed("null"),
        Value::Array(_) | Value::Object(_) => Cow::Owned(value.to_string()),
    }
}

/// JavaScript truthiness, which is how Mountebank reads an `exists` expectation and decides
/// whether a container can hold fields at all: `false`, `null`, `0`, `""` are falsy; non-empty
/// strings, non-zero numbers, objects and arrays are truthy.
pub(crate) fn truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

/// The predicate options an `exists` check reads. `except` is applied to every value before it is
/// tested for empty, as Mountebank's `normalize` runs `exceptTransform` for every operator (#1408).
struct ExistsCheck<'e> {
    except: &'e dyn for<'a> Fn(&'a str) -> Cow<'a, str>,
    key_case_sensitive: bool,
}

impl ExistsCheck<'_> {
    /// Mountebank's `exists` leaf (predicates.js): only a missing value or the empty string, after
    /// `except`, is absent.
    fn present(&self, value: Option<&str>) -> bool {
        value.is_some_and(|v| !(self.except)(v).is_empty())
    }

    /// Whether the expectation asks for the field, by truthiness; a string expectation is
    /// stripped by `except` first, as Mountebank normalizes the expected side too.
    fn expects_present(&self, expected: &serde_json::Value) -> bool {
        match expected {
            serde_json::Value::String(s) => self.present(Some(s)),
            other => truthy(other),
        }
    }

    /// Whether `actual` can hold fields: truthy, with a string judged after `except`.
    fn is_container(&self, actual: &serde_json::Value) -> bool {
        match actual {
            serde_json::Value::String(s) => self.present(Some(s)),
            other => truthy(other),
        }
    }

    /// Mountebank's `exists` over a field holding several values (`onlyActualIsArray`): a truthy
    /// expectation holds outright; a falsy one holds when some value is absent.
    fn holds_for_values(&self, expected: &serde_json::Value, values: &[&str]) -> bool {
        let wanted = self.expects_present(expected);
        match values {
            [] => !wanted,
            [one] => self.present(Some(one)) == wanted,
            many => wanted || many.iter().any(|v| !self.present(Some(v))),
        }
    }

    /// Look up `key` in a JSON object, honouring `keyCaseSensitive` (Mountebank lowercases body
    /// keys unless it is set, `tryJSON`).
    fn field<'a>(&self, actual: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
        let obj = actual.as_object()?;
        if self.key_case_sensitive {
            obj.get(key)
        } else {
            obj.get(key).or_else(|| {
                obj.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v)
            })
        }
    }

    /// Mountebank's `predicateSatisfied` for `exists`: an object expectation against `actual`.
    ///
    /// A falsy `actual` (empty body, missing or `""`/`null`/`0`/`false` parent) fails whatever it
    /// expects. A string is read as JSON when it parses — unstripped, as Rift strips JSON leaves
    /// rather than the raw text (#105); otherwise it has no fields, so every field in it is absent.
    /// An array is tested element by element.
    fn satisfied(
        &self,
        expected: &serde_json::Map<String, serde_json::Value>,
        actual: &serde_json::Value,
    ) -> bool {
        if !self.is_container(actual) {
            return false;
        }
        if let serde_json::Value::String(text) = actual
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text)
        {
            return self.fields(expected, &parsed);
        }
        self.fields(expected, actual)
    }

    fn fields(
        &self,
        expected: &serde_json::Map<String, serde_json::Value>,
        actual: &serde_json::Value,
    ) -> bool {
        if let serde_json::Value::Array(elements) = actual {
            return elements
                .iter()
                .any(|element| self.satisfied(expected, element));
        }
        expected.iter().all(|(key, expected_val)| {
            let field = self.field(actual, key);
            match (expected_val, field) {
                // Deviation from Mountebank, which holds any truthy expectation over an array
                // without looking at it: an object expectation holds when some element satisfies it.
                (serde_json::Value::Object(nested), Some(serde_json::Value::Array(elements))) => {
                    elements
                        .iter()
                        .any(|element| self.satisfied(nested, element))
                }
                (serde_json::Value::Object(nested), Some(value)) => self.satisfied(nested, value),
                (serde_json::Value::Object(_), None) => false,
                (_, Some(serde_json::Value::Array(elements))) => {
                    self.expects_present(expected_val)
                        || elements.iter().any(
                            |e| matches!(e, serde_json::Value::String(s) if !self.present(Some(s))),
                        )
                }
                (_, Some(serde_json::Value::String(text))) => {
                    self.present(Some(text)) == self.expects_present(expected_val)
                }
                (_, field) => field.is_some() == self.expects_present(expected_val),
            }
        })
    }
}

/// Check exists predicate - verifies field presence or absence, as Mountebank's `exists` does:
/// an empty value (after `except`) is absent, a missing container (empty body, no form) fails any
/// object expectation, and the expectation is read for JavaScript truthiness.
/// Supports: method, path, body, query, headers, form, requestFrom, ip
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_exists_predicate<H>(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    // Concretely `FastMap` — always sourced from `parse_query`/`parse_query_string` (issue #704).
    query: &MultiValueMap,
    headers: &H,
    body: &str,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `check_predicate_fields`.
    form: Option<&MultiValueMap>,
    key_case_sensitive: bool,
    // `body` parsed once per request, when it is the compared body (no selector) and is JSON.
    body_json: &crate::behaviors::LazyJson<'_>,
    apply_except: &dyn for<'a> Fn(&'a str) -> Cow<'a, str>,
) -> bool
where
    H: RequestHeaders,
{
    let check = ExistsCheck {
        except: apply_except,
        key_case_sensitive,
    };
    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    for (field, value) in [
        ("method", Some(method)),
        ("path", Some(path)),
        ("requestFrom", request_from),
        ("ip", client_ip),
    ] {
        if let Some(expected) = obj.get(field)
            && check.present(value) != check.expects_present(expected)
        {
            return false;
        }
    }

    if let Some(expected) = obj.get("body") {
        let holds = match expected {
            // An empty body (after `except`) is the falsy container; a non-empty one is truthy
            // whatever it parses to. `satisfied` makes that check itself on the raw text.
            serde_json::Value::Object(fields) => match body_json.get() {
                Some(parsed) => check.present(Some(body)) && check.fields(fields, parsed),
                None => check.satisfied(fields, &serde_json::Value::String(body.to_owned())),
            },
            _ => check.present(Some(body)) == check.expects_present(expected),
        };
        if !holds {
            return false;
        }
    }

    // A repeated query or form key is an array, as in Mountebank (issue #1553): a truthy
    // expectation holds outright, and `except` applies to each value.
    if let Some(expected_query) = obj.get("query").and_then(|v| v.as_object()) {
        for (key, expected) in expected_query {
            let values: Vec<&str> = query
                .iter()
                .filter(|(k, _)| key_matches(key, k))
                .flat_map(|(_, v)| v.iter().map(String::as_str))
                .collect();
            if !check.holds_for_values(expected, &values) {
                return false;
            }
        }
    }

    if let Some(expected_headers) = obj.get("headers").and_then(|v| v.as_object()) {
        for (key, expected) in expected_headers {
            // Mountebank folds a repeated header into an array (`headersMap` `arrayifyIfExists`),
            // except that a value replaces an empty first one rather than joining it.
            let mut values: Vec<&str> = Vec::new();
            for value in headers
                .entries()
                .filter(|(k, _)| key_matches(key, k))
                .flat_map(|(_, values)| values.iter().map(String::as_str))
            {
                match values.as_mut_slice() {
                    [first] if first.is_empty() => *first = value,
                    _ => values.push(value),
                }
            }
            if !check.holds_for_values(expected, &values) {
                return false;
            }
        }
    }

    // Mountebank has a `form` only for a non-empty urlencoded body; without one any `form`
    // expectation fails, `false` included (`predicateSatisfied`: `if (!actual) return false`).
    // The serve path parses an empty urlencoded body to an empty form, which is the same absence.
    if let Some(expected_form) = obj.get("form").and_then(|v| v.as_object()) {
        let Some(actual_form) = form.filter(|f| !(f.is_empty() && body.is_empty())) else {
            return false;
        };
        for (key, expected) in expected_form {
            let values: Vec<&str> = actual_form
                .iter()
                .filter(|(k, _)| key_matches(key, k))
                .flat_map(|(_, v)| v.iter().map(String::as_str))
                .collect();
            if !check.holds_for_values(expected, &values) {
                return false;
            }
        }
    }

    true
}

/// How an operator reads a JSON `null` leaf, which is where Mountebank's operators part ways
/// (`predicates.js` `toString`, issue #1579).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullLeaf {
    /// `equals`: `toString(null)` is `null` itself, so a null expectation holds only for a null
    /// value, and a null value only for a null expectation — never for `""` or `"null"`.
    Identity,
    /// `contains`/`startsWith`/`endsWith`: a null value has no text. Mountebank throws (`indexOf`
    /// of null, a 500); Rift misses (`docs/mountebank/differences.md`). A null expectation reads
    /// `"null"`, as `indexOf` coerces its argument.
    NoText,
    /// `matches`: `new RegExp(null)` and `test(null)` both read `"null"`, and an object reads
    /// `"[object Object]"`.
    Text,
}

/// Mountebank's `predicateSatisfied` (`predicates.js:257-299`) for `equals`, `contains`,
/// `startsWith`, `endsWith` and `matches` over a field holding JSON: one per predicate, holding
/// what the operator compares leaves with.
///
/// Per expected field, in Mountebank's order:
///
/// 1. the actual value is an array → [`satisfied_by_array`]: an expected array is a subset in any
///    order, anything else needs some element (`allExpectedArrayValuesMatchActualArray`,
///    `expectedMatchesAtLeastOneValueInActualArray`);
/// 2. the actual container is an array with no such field, and the expectation is not an array →
///    some element satisfies the **whole** expected container
///    (`expectedLeftOffArraySyntaxButActualIsArrayOfObjects`), so `{"name": "x"}` matches
///    `[{"name": "y"}, {"name": "x"}]`;
/// 3. the expectation is a container → recurse. A string value is read as JSON at every level
///    (`tryJSON`). An expected array recurses **by index** against an actual array, after sorting
///    the expected side as `normalize` does — which is why a top-level `body: ["a"]` matches the
///    prefix of `["a", "b"]` and `["a", "b"]` misses `["b", "a"]`;
/// 4. otherwise the leaf is compared as text ([`leaf_text`]); a missing field reads `""`.
///
/// Deliberately not reproduced: an expected array against a plain (non-JSON) string, which
/// Mountebank compares with the string's characters (`Object.keys` over a string), misses.
pub(crate) struct Satisfy<'a> {
    /// The operator over (expected text, actual text). The expected side arrives as written; the
    /// caller's closure applies `except` to it where Mountebank does.
    pub(crate) compare: &'a dyn Fn(&str, &str) -> bool,
    /// Applied to every actual leaf (Rift strips leaves, not the raw JSON text, issue #105).
    pub(crate) except: &'a dyn for<'b> Fn(&'b str) -> Cow<'b, str>,
    pub(crate) key_case_sensitive: bool,
    /// Only orders an expected array compared by index: `normalize` sorts the lowercased values.
    pub(crate) case_sensitive: bool,
    pub(crate) nulls: NullLeaf,
}

impl Satisfy<'_> {
    /// An expected container against a request field's text — the body, or a query value —
    /// with `pre_parsed` the once-per-request parse of that text when the caller has it (#290).
    pub(crate) fn text(&self, expected: &Value, text: &str, pre_parsed: Option<&Value>) -> bool {
        // `if (!actual) return false`: an empty field holds no fields.
        if text.is_empty() {
            return false;
        }
        match pre_parsed {
            Some(parsed) => self.parsed(expected, parsed),
            None => match serde_json::from_str::<Value>(text) {
                Ok(parsed) => self.parsed(expected, &parsed),
                // Domain-optional parse: text that is not JSON is a plain string, which has no
                // fields — each expected field reads as missing.
                Err(_) => self.fields(expected, &Value::String(text.to_owned())),
            },
        }
    }

    /// `predicateSatisfied(expected, actual)` for an expected container; `None` is a missing field.
    fn container(&self, expected: &Value, actual: Option<&Value>) -> bool {
        match actual {
            Some(Value::String(text)) => self.text(expected, text, None),
            Some(actual) if truthy(actual) => self.fields(expected, actual),
            // A missing, null, `0` or `false` value holds no fields.
            _ => false,
        }
    }

    /// After `tryJSON`. A body that is the JSON `null` makes Mountebank throw reading its fields;
    /// Rift misses.
    fn parsed(&self, expected: &Value, parsed: &Value) -> bool {
        !parsed.is_null() && self.fields(expected, parsed)
    }

    /// `Object.keys(expected).every(...)` over an actual value that is no longer JSON text.
    fn fields(&self, expected: &Value, actual: &Value) -> bool {
        match expected {
            Value::Object(fields) => fields.iter().all(|(key, expected_value)| {
                let value = object_field(actual, key, self.key_case_sensitive);
                self.field(expected, expected_value, value, actual)
            }),
            Value::Array(elements) => {
                // Index by position only into an array: no character indexing of a string.
                let Value::Array(actuals) = actual else {
                    return false;
                };
                self.sorted(elements)
                    .into_iter()
                    .enumerate()
                    .all(|(i, element)| self.field(expected, element, actuals.get(i), actual))
            }
            leaf => self.leaf(leaf, Some(actual)),
        }
    }

    /// One field of `whole`: rules 1–4 of [`Satisfy`].
    fn field(
        &self,
        whole: &Value,
        expected: &Value,
        actual: Option<&Value>,
        parent: &Value,
    ) -> bool {
        match (actual, parent) {
            (Some(Value::Array(actuals)), _) => {
                satisfied_by_array(expected, actuals, |e, a| self.test(e, Some(a)))
            }
            (None, Value::Array(elements)) if !expected.is_array() => elements
                .iter()
                .any(|element| self.test(whole, Some(element))),
            _ => self.test(expected, actual),
        }
    }

    /// `testPredicate`: a container recurses, a leaf compares.
    fn test(&self, expected: &Value, actual: Option<&Value>) -> bool {
        match expected {
            Value::Object(_) | Value::Array(_) => self.container(expected, actual),
            leaf => self.leaf(leaf, actual),
        }
    }

    /// A scalar expectation against a JSON value; `None` is a missing field, which reads `""`.
    fn leaf(&self, expected: &Value, actual: Option<&Value>) -> bool {
        let actual: Cow<'_, str> = match actual {
            None => Cow::Borrowed(""),
            Some(Value::Null) => match self.nulls {
                NullLeaf::Identity => return expected.is_null(),
                NullLeaf::NoText => return false,
                NullLeaf::Text => Cow::Borrowed("null"),
            },
            // `String(object)`; only a pattern can hold against it. An array element that is
            // itself an array has no text Rift compares.
            Some(Value::Object(_)) if self.nulls == NullLeaf::Text => {
                Cow::Borrowed("[object Object]")
            }
            Some(Value::Object(_) | Value::Array(_)) => return false,
            Some(scalar) => leaf_text(scalar),
        };
        self.leaf_against_text(expected, &actual)
    }

    /// A scalar expectation against text: a request field (the body, a header value) or a JSON
    /// leaf's [`leaf_text`].
    pub(crate) fn leaf_against_text(&self, expected: &Value, actual: &str) -> bool {
        if expected.is_null() && self.nulls == NullLeaf::Identity {
            return false;
        }
        (self.compare)(&leaf_text(expected), &(self.except)(actual))
    }

    /// The expected array in `normalize`'s order (`sortObjects`): numbers numerically, everything
    /// else by its text — lowercased unless `caseSensitive` — with a container as its JSON.
    fn sorted<'v>(&self, elements: &'v [Value]) -> Vec<&'v Value> {
        let key = |v: &Value| -> String {
            let text = leaf_text(v);
            if self.case_sensitive {
                text.into_owned()
            } else {
                text.to_ascii_lowercase()
            }
        };
        let mut sorted: Vec<&Value> = elements.iter().collect();
        sorted.sort_by(|a, b| match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            _ => key(a).cmp(&key(b)),
        });
        sorted
    }
}

/// `actual[key]` for a JSON object, honouring `keyCaseSensitive` (Mountebank lowercases keys on
/// both sides unless it is set). Anything else has no fields.
fn object_field<'a>(actual: &'a Value, key: &str, key_case_sensitive: bool) -> Option<&'a Value> {
    let fields = actual.as_object()?;
    if key_case_sensitive {
        fields.get(key)
    } else {
        fields.get(key).or_else(|| {
            fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        })
    }
}

/// Mountebank's `deepEquals` (`predicates.js:301-314`) for a field holding JSON: both sides are
/// brought to one canonical form and compared whole.
///
/// The canonical form is `forceStrings` + `normalize`: every leaf becomes its [`leaf_text`]
/// (`null` → `"null"`, `1.0` → `"1"`), stripped by `except` and lowercased unless
/// `caseSensitive`; keys are lowercased unless `keyCaseSensitive`; and every array is sorted, so
/// arrays compare as multisets (`["a", "b"]` equals `["b", "a"]`, `["a", "a", "b"]` does not
/// equal `["a", "b", "b"]`). The request text is parsed once (`tryJSON`), and a JSON string nested
/// inside it stays a string.
pub(crate) struct DeepEquals<'a> {
    pub(crate) except: &'a dyn for<'b> Fn(&'b str) -> Cow<'b, str>,
    pub(crate) case_sensitive: bool,
    pub(crate) key_case_sensitive: bool,
}

/// [`DeepEquals`]'s canonical form. Every leaf is text, and `Ord` is derived only so arrays can be
/// sorted: any total order makes sorted equality multiset equality.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Canonical {
    Text(String),
    List(Vec<Canonical>),
    Map(BTreeMap<String, Canonical>),
}

impl DeepEquals<'_> {
    /// An expected container against a request field's text, `pre_parsed` being its parse when
    /// the caller has one. Text that is not JSON is a string, which no container equals.
    pub(crate) fn text(&self, expected: &Value, text: &str, pre_parsed: Option<&Value>) -> bool {
        let actual = match pre_parsed {
            Some(parsed) => self.canonical(parsed),
            None => match serde_json::from_str::<Value>(text) {
                Ok(parsed) => self.canonical(&parsed),
                // Domain-optional parse: non-JSON text is a string, never equal to a container.
                Err(_) => return false,
            },
        };
        self.canonical(expected) == actual
    }

    fn canonical(&self, value: &Value) -> Canonical {
        match value {
            Value::Object(fields) => Canonical::Map(
                // A later key that folds onto an earlier one replaces it, as `reduce` does.
                fields
                    .iter()
                    .map(|(key, v)| (self.fold_key(key), self.canonical(v)))
                    .collect(),
            ),
            Value::Array(elements) => {
                let mut list: Vec<Canonical> = elements.iter().map(|v| self.canonical(v)).collect();
                list.sort();
                Canonical::List(list)
            }
            leaf => {
                let text = leaf_text(leaf);
                let text = (self.except)(&text);
                Canonical::Text(if self.case_sensitive {
                    text.into_owned()
                } else {
                    text.to_ascii_lowercase()
                })
            }
        }
    }

    fn fold_key(&self, key: &str) -> String {
        if self.key_case_sensitive {
            key.to_owned()
        } else {
            key.to_ascii_lowercase()
        }
    }
}

/// Whether a string could itself be JSON — trimmed, it starts with `{` or `[`.
///
/// [`Satisfy`] reads such a string as the container it encodes, so the quamina body-field
/// dimension cannot match it structurally and widens instead. [`DeepEquals`] no longer re-parses
/// nested strings (issue #1579), so for the body-hash dimension the bail only widens; dropping it
/// there is an optimisation, kept behind the property test. See [`structural_hash`].
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

/// Order-sensitive combine of two sub-hashes (key↔value within an object entry, tags).
fn combine(a: u64, b: u64) -> u64 {
    mix64(a ^ mix64(b).rotate_left(31))
}

/// Hash a string leaf under the comparator's default fold.
///
/// `FixedState` (not `RandomState`): this `u64` is *stored* in the body-index map and computed at
/// two different times (index build vs request), so it must be stable within the process. Stability
/// *across* processes is not required and not provided.
fn hash_folded(s: &str) -> u64 {
    // Default (non-`caseSensitive`) deepEquals lowercases leaves ASCII-only, so folding with
    // `to_ascii_lowercase` before hashing makes case-equal leaves hash-equal — exactly and only
    // ASCII, matching the evaluator (Unicode folding would over-collapse; see `stub_index::fold`).
    foldhash::fast::FixedState::default().hash_one(s.to_ascii_lowercase())
}

/// Container type tags, keeping `{}`/`[]`/leaves in distinct hash spaces. Leaves carry **no** tag,
/// so `forceStrings`' coercion (`1`≡`"1"`, `true`≡`"true"`, `null`≡`"null"`) is preserved.
const OBJ_TAG: u64 = 0x9e37_79b9_7f4a_7c15;
const ARR_TAG: u64 = 0xc2b2_ae3d_27d4_eb4f;

/// A structural hash of a JSON value under the **deepEquals default-mode** comparator's equivalence
/// relation — the one the body-hash index (#708) probes with: [`DeepEquals`] with no `except`,
/// case folded, keys folded. It hashes that comparator's canonical form, NOT `serde_json::Value::eq`:
///
/// - **Leaves** hash `fold(leaf_text(v))`, so the coercions `forceStrings` makes collapse to one
///   bucket: `1`≡`"1"`≡`1.0`, `true`≡`"true"`, `null`≡`"null"`, and ASCII-case-equal strings.
/// - **Objects** combine commutatively over `combine(hash(fold(key)), hash(value))`, plus a length
///   tag, after folding keys — a later key that folds onto an earlier one replaces it, as the
///   canonical form does.
/// - **Arrays** combine **commutatively** too, plus a length tag (issue #1579): deepEquals compares
///   arrays as multisets, so an order-sensitive hash would prune the stub a reordered body matches.
///   Each element is `mix64`'d before the wrapping add, so duplicates count.
///
/// Returns `None` when the value contains a JSON-ish string leaf (see [`is_jsonish_string`]); the
/// caller treats `None` as "not indexable" at build time and "do not prune this dimension" at
/// request time — both of which only ever *widen* the candidate set, so soundness is preserved.
///
/// Collisions are harmless: the index over-approximates and Stage-2 ([`DeepEquals`]) rejects false
/// candidates. The one real bug class is *under*-approximation — a value equal under the comparator
/// hashing differently — which is what the `structural_hash` property test guards.
pub(crate) fn structural_hash(value: &Value) -> Option<u64> {
    match value {
        Value::Object(map) => {
            let mut folded: BTreeMap<String, &Value> = BTreeMap::new();
            for (k, v) in map {
                folded.insert(k.to_ascii_lowercase(), v);
            }
            let mut acc: u64 = 0;
            for (k, v) in &folded {
                let entry = combine(hash_folded(k), structural_hash(v)?);
                // Commutative (wrapping-add) so key order does not affect the hash, but each entry
                // is `mix64`'d first so `{"a":"b"}` and `{"b":"a"}` do not cancel.
                acc = acc.wrapping_add(mix64(entry));
            }
            Some(combine(OBJ_TAG, combine(folded.len() as u64, acc)))
        }
        Value::Array(arr) => {
            let mut acc: u64 = 0;
            for v in arr {
                acc = acc.wrapping_add(mix64(structural_hash(v)?));
            }
            Some(combine(ARR_TAG, combine(arr.len() as u64, acc)))
        }
        Value::String(s) if is_jsonish_string(s) => None,
        // String (non-JSON-ish), Number, Bool, Null: the comparator compares the *stringified* leaf,
        // so hash that same normalization.
        leaf => Some(hash_folded(&leaf_text(leaf))),
    }
}

#[cfg(test)]
mod structural_hash_tests {
    use super::*;
    use serde_json::{Value, json};

    /// deepEquals in default mode (the exact relation the body index indexes): case-insensitive
    /// leaves, case-insensitive keys, no `except`, no selector.
    fn deep_equals_default(expected: &Value, actual: &Value) -> bool {
        DeepEquals {
            except: &|s: &str| Cow::Borrowed(s),
            case_sensitive: false,
            key_case_sensitive: false, // defaults to caseSensitive = false
        }
        .text(expected, &actual.to_string(), Some(actual))
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
            (json!({"z": null}), json!({"z": "null"})), // null ≡ "null" (forceStrings)
            (json!({"n": 1}), json!({"n": 1.0})), // JS number spelling
            (json!({"n": 100}), json!({"n": 1e2})), // JS number spelling
            (json!({"s": "ABC"}), json!({"s": "abc"})), // ASCII case fold
            (json!({"a": 1, "b": 2}), json!({"b": 2, "a": 1})), // object key order
            (json!({"Key": 1}), json!({"key": 1})), // key case (keyCaseSensitive default false)
            (json!([1, "2", null]), json!(["1", 2, "null"])), // nested array coercions
            (json!([1, 2]), json!([2, 1])),       // arrays are multisets
            (
                json!({"i": [{"id": 1}, {"id": 2}]}),
                json!({"i": [{"id": 2}, {"id": 1}]}),
            ),
            (json!({"k": 1, "K": 1}), json!({"k": 1})), // keys that fold together are one key
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
    fn distinct_values_stay_distinct() {
        // Values deepEquals tells apart should not collide (a collision is only a missed prune).
        for (e, a) in [
            (json!({"n": 1.5}), json!({"n": 1})),
            (json!({"z": null}), json!({"z": ""})),
            (json!(["a", "a", "b"]), json!(["a", "b", "b"])), // duplicates count
            (json!(["a"]), json!(["a", "a"])),
        ] {
            assert!(
                !deep_equals_default(&e, &a),
                "precondition: {e} differs from {a}"
            );
            assert_ne!(
                structural_hash(&e),
                structural_hash(&a),
                "{e} and {a} collide"
            );
        }
    }

    #[test]
    fn json_in_string_leaf_bails_both_directions() {
        // A string leaf that could be JSON is equivalent to the container it encodes, in both
        // directions — structural_hash must return None so the caller widens rather than prunes.
        assert_eq!(structural_hash(&json!({"a": "{\"b\":1}"})), None);
        assert_eq!(structural_hash(&json!({"a": " [1,2]"})), None); // leading space, still JSON-ish
        assert_eq!(structural_hash(&json!(["{}"])), None);
        // deepEquals parses the request once, as Mountebank's `tryJSON` does, so a nested JSON
        // string stays a string (issue #1579); the bail now only widens.
        assert!(!deep_equals_default(
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
    fn array_order_is_ignored() {
        // deepEquals compares arrays as multisets (issue #1579), so the hash must ignore order too:
        // an order-sensitive hash prunes the stub a reordered body matches.
        for (e, a) in [
            (json!([1, 2]), json!([2, 1])),
            (json!({"m": [[2, 1], [3]]}), json!({"m": [[3], [1, 2]]})),
            (
                json!([{"a": null}, {"a": "z"}]),
                json!([{"a": "z"}, {"a": "null"}]),
            ),
        ] {
            assert!(
                deep_equals_default(&e, &a),
                "precondition: {e} deep-equals {a}"
            );
            assert_eq!(structural_hash(&e), structural_hash(&a), "{e} and {a}");
        }
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
        // From the high bits: an LCG's low bits cycle with a short period, which left the
        // generator never producing a reordered array of two or more distinct elements.
        fn below(&mut self, n: u64) -> u64 {
            (self.next_u64() >> 33) % n
        }
        fn coin(&mut self) -> bool {
            self.next_u64() >> 63 == 1
        }
    }

    fn gen_value(rng: &mut Lcg, depth: u32) -> Value {
        // Leaf bias grows as the remaining depth shrinks, so trees terminate. (It used to grow
        // with the remaining depth, which made every top-level value a leaf: the property never
        // saw a container until issue #1579.)
        let leaf = depth == 0 || rng.below(100) < 70u64.saturating_sub(u64::from(depth) * 20);
        if leaf {
            match rng.below(9) {
                0 => Value::Null,
                1 => Value::Bool(rng.coin()),
                2 => json!(rng.below(4) as i64), // small ints so "n" ≡ "\"n\"" pairs recur
                3 => json!(rng.below(3) as f64 + 0.5), // non-integer floats
                7 => json!(rng.below(3) as f64), // integral floats ≡ ints (JS spelling)
                8 => Value::String("NULL".to_string()), // ≡ null once folded
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
            let n = rng.below(4);
            Value::Array((0..n).map(|_| gen_value(rng, depth - 1)).collect())
        }
    }

    /// Produce a value that deep-equals `v` in default mode (coerce scalars across types, flip ASCII
    /// case, permute object keys and array elements) so the property test actually exercises
    /// equality, not just the vacuous `false` branch.
    fn preserving_variant(rng: &mut Lcg, v: &Value) -> Value {
        match v {
            Value::Null => {
                if rng.coin() {
                    Value::String("null".to_string())
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
                    Value::String(rift_types::wire::js_number_string(n))
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
            Value::Array(a) => {
                // Arrays are multisets: rotate the elements by a non-zero amount.
                let mut elements: Vec<Value> =
                    a.iter().map(|e| preserving_variant(rng, e)).collect();
                if elements.len() > 1 {
                    let by = 1 + rng.below(elements.len() as u64 - 1);
                    elements.rotate_left(by as usize);
                }
                Value::Array(elements)
            }
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

/// Mountebank's two JSON comparators, rule by rule (issue #1579). Each case was answered by
/// Mountebank 2.9.5; `conformance/differential/cases/body_arrays.json` replays them against it.
#[cfg(test)]
mod comparator_tests {
    use super::*;
    use serde_json::json;

    fn no_except(s: &str) -> Cow<'_, str> {
        Cow::Borrowed(s)
    }

    /// `equals` in default mode (case-insensitive, keys folded, no `except`).
    fn equals(expected: Value, body: &str) -> bool {
        Satisfy {
            compare: &|e: &str, a: &str| e.eq_ignore_ascii_case(a),
            except: &no_except,
            key_case_sensitive: false,
            case_sensitive: false,
            nulls: NullLeaf::Identity,
        }
        .text(&expected, body, None)
    }

    fn contains(expected: Value, body: &str) -> bool {
        Satisfy {
            compare: &|e: &str, a: &str| a.contains(e),
            except: &no_except,
            key_case_sensitive: false,
            case_sensitive: false,
            nulls: NullLeaf::NoText,
        }
        .text(&expected, body, None)
    }

    fn matches(expected: Value, body: &str) -> bool {
        Satisfy {
            compare: &|p: &str, a: &str| regex::Regex::new(p).is_ok_and(|re| re.is_match(a)),
            except: &no_except,
            key_case_sensitive: false,
            case_sensitive: true,
            nulls: NullLeaf::Text,
        }
        .text(&expected, body, None)
    }

    fn deep_equals(expected: Value, body: &str) -> bool {
        DeepEquals {
            except: &no_except,
            case_sensitive: false,
            key_case_sensitive: false,
        }
        .text(&expected, body, None)
    }

    #[test]
    fn an_expected_array_is_a_subset_of_an_actual_array_in_any_order() {
        assert!(equals(
            json!({"tags": ["a", "c"]}),
            r#"{"tags": ["a", "b", "c"]}"#
        ));
        assert!(equals(json!({"t": ["b", "a"]}), r#"{"t": ["a", "b"]}"#));
        assert!(equals(json!({"t": []}), r#"{"t": ["a"]}"#));
        assert!(equals(json!({"t": [null]}), r#"{"t": [null, "a"]}"#));
        assert!(equals(
            json!({"i": [{"id": 2}]}),
            r#"{"i": [{"id": 1, "x": 0}, {"id": 2, "x": 1}]}"#
        ));
        // A superset expectation misses.
        assert!(!equals(json!({"t": ["a", "z"]}), r#"{"t": ["a"]}"#));
    }

    #[test]
    fn a_scalar_or_object_expectation_holds_for_any_element() {
        assert!(equals(json!({"t": "b"}), r#"{"t": ["a", "b"]}"#));
        assert!(equals(
            json!({"i": {"id": 2}}),
            r#"{"i": [{"id": 1}, {"id": 2}]}"#
        ));
        assert!(!equals(
            json!({"i": {"id": 3}}),
            r#"{"i": [{"id": 1}, {"id": 2}]}"#
        ));
        assert!(contains(json!({"t": ["b"]}), r#"{"t": ["xa", "xbx"]}"#));
        assert!(matches(json!({"t": "^b$"}), r#"{"t": ["a", "b"]}"#));
        assert!(matches(
            json!({"t": ["^a", "^b"]}),
            r#"{"t": ["bz", "az", "c"]}"#
        ));
    }

    #[test]
    fn an_object_left_off_array_syntax_matches_any_element_of_an_array_of_objects() {
        assert!(equals(
            json!({"name": "x"}),
            r#"[{"name": "y"}, {"name": "x"}]"#
        ));
        // The whole object must hold for one element; fields split across elements miss.
        assert!(!equals(
            json!({"a": "1", "b": "2"}),
            r#"[{"a": "1"}, {"b": "2"}]"#
        ));
        assert!(!equals(json!({"a": "1"}), r#"["x"]"#));
    }

    #[test]
    fn a_top_level_expected_array_compares_by_position_after_sorting_the_expected_side() {
        assert!(equals(json!(["a"]), r#"["a", "b"]"#)); // a prefix
        assert!(equals(json!(["b", "a"]), r#"["a", "b"]"#)); // expected sorted
        assert!(!equals(json!(["a", "b"]), r#"["b", "a"]"#)); // the actual side is not
        assert!(!equals(json!(["b"]), r#"["a", "b"]"#));
    }

    #[test]
    fn contains_does_not_match_across_elements() {
        assert!(!contains(json!({"t": "a\",\"b"}), r#"{"t": ["a", "b"]}"#));
    }

    #[test]
    fn null_is_only_null_under_equals() {
        assert!(equals(json!({"a": null}), r#"{"a": null}"#));
        assert!(!equals(json!({"a": null}), r#"{"a": ""}"#));
        assert!(!equals(json!({"a": null}), r#"{"a": "null"}"#));
        assert!(!equals(json!({"a": null}), r#"{"b": 1}"#));
        assert!(!equals(json!({"a": ""}), r#"{"a": null}"#));
        // A null value has no text for contains (Mountebank throws), and reads "null" to a regex.
        assert!(!contains(json!({"a": "x"}), r#"{"a": null}"#));
        assert!(matches(json!({"a": "^null$"}), r#"{"a": null}"#));
        assert!(!matches(json!({"a": "^$"}), r#"{"a": null}"#));
    }

    #[test]
    fn a_missing_field_reads_as_the_empty_string() {
        assert!(equals(json!({"a": ""}), r#"{"b": 1}"#));
        assert!(equals(json!({"a": ""}), "plain text"));
        // A missing container holds no fields.
        assert!(!equals(json!({"a": {"b": ""}}), r#"{"c": 1}"#));
        assert!(!equals(json!({"a": ""}), ""));
    }

    #[test]
    fn numbers_compare_as_javascript_prints_them() {
        assert!(equals(json!({"a": 1}), r#"{"a": 1.0}"#));
        assert!(equals(json!({"a": 100}), r#"{"a": 1e2}"#));
        assert!(equals(json!({"a": 1}), r#"{"a": "1"}"#));
        assert!(deep_equals(json!({"a": 1}), r#"{"a": 1.0}"#));
        assert!(deep_equals(json!({"a": 100}), r#"{"a": 1e2}"#));
    }

    #[test]
    fn an_expected_array_against_a_plain_string_misses() {
        // Mountebank indexes the string's characters here; Rift does not (differences.md).
        assert!(!equals(json!({"t": ["a"]}), r#"{"t": "a"}"#));
        assert!(!equals(json!({"t": ["a"]}), r#"{"t": "abc"}"#));
        // A JSON string that encodes an array is read as one, by position.
        assert!(equals(json!({"s": ["a"]}), r#"{"s": "[\"a\", \"b\"]"}"#));
        assert!(!equals(json!({"s": ["b"]}), r#"{"s": "[\"a\", \"b\"]"}"#));
    }

    #[test]
    fn deep_equals_compares_arrays_as_multisets() {
        assert!(deep_equals(
            json!({"items": [{"id": 1}, {"id": 2}]}),
            r#"{"items": [{"id": 2}, {"id": 1}]}"#
        ));
        assert!(deep_equals(
            json!({"m": [[2, 1], [3]]}),
            r#"{"m": [[3], [1, 2]]}"#
        ));
        assert!(deep_equals(json!([1, 2]), "[2, 1]"));
        assert!(deep_equals(
            json!({"t": [{"k": 1}, "s"]}),
            r#"{"t": ["s", {"k": 1}]}"#
        ));
        assert!(deep_equals(
            json!({"t": ["B", "a"]}),
            r#"{"t": ["b", "A"]}"#
        ));
        // Duplicates count, length is exact, and an element's extra key is a difference.
        assert!(!deep_equals(
            json!({"t": ["a", "a", "b"]}),
            r#"{"t": ["a", "b", "b"]}"#
        ));
        assert!(!deep_equals(json!({"t": ["a"]}), r#"{"t": ["a", "a"]}"#));
        assert!(!deep_equals(
            json!({"i": [{"id": 1}]}),
            r#"{"i": [{"id": 1, "x": 2}]}"#
        ));
    }

    #[test]
    fn deep_equals_reads_null_as_the_text_null() {
        assert!(deep_equals(
            json!({"a": null, "b": "1"}),
            r#"{"a": "null", "b": "1"}"#
        ));
        assert!(deep_equals(json!({"a": "null"}), r#"{"a": null}"#));
        assert!(deep_equals(
            json!([{"a": null}, {"a": "z"}]),
            r#"[{"a": "z"}, {"a": "null"}]"#
        ));
        assert!(!deep_equals(json!({"a": null}), r#"{"a": ""}"#));
    }

    #[test]
    fn deep_equals_parses_the_request_once() {
        // A JSON string nested in the body stays a string.
        assert!(!deep_equals(
            json!({"a": {"b": "1"}}),
            r#"{"a": "{\"b\": 1}"}"#
        ));
        assert!(!deep_equals(json!({"a": 1}), "not json"));
    }

    #[test]
    fn deep_equals_applies_except_and_case_to_every_leaf() {
        fn strip_x(s: &str) -> Cow<'_, str> {
            Cow::Owned(s.replace('x', ""))
        }
        let sensitive = DeepEquals {
            except: &no_except,
            case_sensitive: true,
            key_case_sensitive: true,
        };
        assert!(sensitive.text(&json!({"t": ["B", "a"]}), r#"{"t": ["a", "B"]}"#, None));
        assert!(!sensitive.text(&json!({"t": ["b", "a"]}), r#"{"t": ["a", "B"]}"#, None));
        let except = DeepEquals {
            except: &strip_x,
            case_sensitive: false,
            key_case_sensitive: false,
        };
        assert!(except.text(&json!({"t": ["b", "a"]}), r#"{"t": ["ax", "bx"]}"#, None));
    }
}
