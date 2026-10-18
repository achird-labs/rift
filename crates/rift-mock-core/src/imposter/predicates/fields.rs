//! Field-oriented predicate checks (`equals`/`contains`/`startsWith`/`endsWith`/`matches`)
//! that compare a request's extracted fields against predicate values.

use super::RequestHeaders;
use super::arrays::{deep_equals_array, satisfied_by_array};
use super::json::{DeepEquals, NullLeaf, Satisfy, leaf_text};
use super::regex_cache::cached_regex;
use crate::util::MultiValueMap;
use std::collections::HashMap;

/// One keyed field — `query`, `form` or `headers` — whose keys may each carry several values
/// (issue #1553): a repeated query or form key, or a repeated header. A key sent once is compared
/// as a scalar by `check_value`; a key sent several times is an array, compared by Mountebank's
/// array rule ([`super::arrays`]), so a scalar expectation holds when any value satisfies it and an
/// array one needs each of its elements among the values. `deepEquals` also needs the same set of
/// keys, and a repeated key the same values as a multiset.
fn keyed_field_holds<M: RequestHeaders>(
    expected: &serde_json::Map<String, serde_json::Value>,
    actual: &M,
    deep_equals: bool,
    key_matches: &impl Fn(&str, &str) -> bool,
    check_value: &impl Fn(&serde_json::Value, &str) -> bool,
) -> bool {
    if deep_equals && expected.len() != actual.len() {
        return false;
    }
    expected.iter().all(|(key, expected_val)| {
        let Some((_, values)) = actual.entries().find(|(k, _)| key_matches(key, k)) else {
            return false;
        };
        let test = |element: &serde_json::Value, value: &String| check_value(element, value);
        match values {
            [one] => check_value(expected_val, one),
            many if deep_equals => deep_equals_array(expected_val, many, test),
            many => satisfied_by_array(expected_val, many, test),
        }
    })
}

/// Check predicate fields against request values
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_predicate_fields<F, H>(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    // Concretely `FastMap` — always sourced from `parse_query`/`parse_query_string` (issue #704);
    // every value of a repeated key (issue #1553).
    query: &MultiValueMap,
    headers: &H,
    body: &str,
    apply_except: &impl for<'a> Fn(&'a str) -> std::borrow::Cow<'a, str>,
    compare: F,
    deep_equals: bool,
    // `caseSensitive`, and how the operator reads a JSON `null` leaf (issue #1579).
    case_sensitive: bool,
    nulls: NullLeaf,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `predicate_matches_inner`.
    form: Option<&MultiValueMap>,
    key_case_sensitive: bool,
    // The request body, parsed at most once per request (issue #290) and only when an object or
    // array `body` expectation needs the tree (#1442); absent for a selector predicate.
    body_json: &crate::behaviors::LazyJson<'_>,
) -> bool
where
    F: Fn(&str, &str) -> bool,
    H: RequestHeaders,
{
    // Mountebank normalizes the expected value with the predicate's `except` as well as the actual
    // one (issue #1329), so a raw value plus `except` — what a predicate generator records — matches
    // every request differing only in the stripped part. `matches` is the exception: its expected
    // value is a pattern (`check_predicate_fields_regex`).
    let compare = |expected: &str, actual: &str| compare(&apply_except(expected), actual);

    // What a field other than the body passes: only the body has a parse to share.
    let absent = crate::behaviors::LazyJson::absent();

    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    // Mountebank's two comparators (issue #1579): `deepEquals` canonicalises both sides and
    // compares them whole; the other operators walk the expectation (`predicateSatisfied`).
    // `except` is applied to leaf values, not to the raw JSON text (#105).
    let satisfy = Satisfy {
        compare: &compare,
        except: apply_except,
        key_case_sensitive,
        case_sensitive,
        nulls,
    };
    let deep = DeepEquals {
        except: apply_except,
        case_sensitive,
        key_case_sensitive,
    };

    // Helper: check a string field against expected value. An object or array expectation reads
    // the field as JSON; a scalar one compares its text.
    let check_string_field = |expected: &serde_json::Value,
                              actual: &str,
                              pre_parsed: &crate::behaviors::LazyJson<'_>|
     -> bool {
        match expected {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) if deep_equals => {
                deep.text(expected, actual, pre_parsed.get())
            }
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                satisfy.text(expected, actual, pre_parsed.get())
            }
            // `forceStrings`: a null expectation is the text `null` under deepEquals.
            _ if deep_equals => compare(&leaf_text(expected), &apply_except(actual)),
            _ => satisfy.leaf_against_text(expected, actual),
        }
    };

    // Check method
    if let Some(expected) = obj.get("method")
        && !check_string_field(expected, method, &absent)
    {
        return false;
    }

    // Check path
    if let Some(expected) = obj.get("path")
        && !check_string_field(expected, path, &absent)
    {
        return false;
    }

    // Check body
    if let Some(expected) = obj.get("body")
        && !check_string_field(expected, body, body_json)
    {
        return false;
    }

    // Check requestFrom (IP:port) - Mountebank compatible
    if let Some(expected) = obj.get("requestFrom") {
        let actual = request_from.unwrap_or("");
        if !check_string_field(expected, actual, &absent) {
            return false;
        }
    }

    // Check ip (just the IP address) - Mountebank compatible
    if let Some(expected) = obj.get("ip") {
        let actual = client_ip.unwrap_or("");
        if !check_string_field(expected, actual, &absent) {
            return false;
        }
    }

    // Form, query and headers: each key may carry several values (issues #994, #1553). A request
    // without a urlencoded body has no form, so every form key is missing.
    let check_value =
        |expected: &serde_json::Value, actual: &str| check_string_field(expected, actual, &absent);
    if let Some(expected) = obj.get("form").and_then(|v| v.as_object())
        && !keyed_field_holds(
            expected,
            form.unwrap_or(&MultiValueMap::default()),
            deep_equals,
            &key_matches,
            &check_value,
        )
    {
        return false;
    }
    if let Some(expected) = obj.get("query").and_then(|v| v.as_object())
        && !keyed_field_holds(expected, query, deep_equals, &key_matches, &check_value)
    {
        return false;
    }
    if let Some(expected) = obj.get("headers").and_then(|v| v.as_object())
        && !keyed_field_holds(expected, headers, deep_equals, &key_matches, &check_value)
    {
        return false;
    }

    true
}

/// Check predicate fields with regex matching
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_predicate_fields_regex<H>(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    // Concretely `FastMap` — see `check_predicate_fields`.
    query: &MultiValueMap,
    headers: &H,
    body: &str,
    apply_except: &impl for<'a> Fn(&'a str) -> std::borrow::Cow<'a, str>,
    case_sensitive: bool,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    // Concretely `FastMap` — see `check_predicate_fields`.
    form: Option<&MultiValueMap>,
    key_case_sensitive: bool,
    // Request body already parsed once per request (issue #290); see `check_predicate_fields`.
    body_json: &crate::behaviors::LazyJson<'_>,
) -> bool
where
    H: RequestHeaders,
{
    // Compile-once, cached regex keyed on (pattern, case_insensitive). Returns `None` for an
    // unparseable pattern, which callers treat as "no match" — same as the previous per-request
    // `Regex::new` returning `Err`.
    let build_regex = |pattern: &str| cached_regex(pattern, !case_sensitive);

    // What a field other than the body passes: only the body has a parse to share.
    let absent = crate::behaviors::LazyJson::absent();

    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    // Helper: check a field against expected value for regex matching, through Mountebank's
    // `predicateSatisfied` like the other operators (issue #1579). A pattern is a leaf's text
    // (`leaf_text` borrows a string pattern, issue #294); the regex is compiled once via
    // `cached_regex`.
    let regex_compare = |pattern: &str, actual: &str| -> bool {
        build_regex(pattern).is_some_and(|re| re.is_match(actual))
    };
    let satisfy = Satisfy {
        compare: &regex_compare,
        except: apply_except,
        key_case_sensitive,
        // `matches` normalizes its patterns case sensitively, so they are ordered as written.
        case_sensitive: true,
        nulls: NullLeaf::Text,
    };
    let check_regex_field = |expected: &serde_json::Value,
                             actual: &str,
                             pre_parsed: &crate::behaviors::LazyJson<'_>|
     -> bool {
        match expected {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                satisfy.text(expected, actual, pre_parsed.get())
            }
            _ => satisfy.leaf_against_text(expected, actual),
        }
    };

    // Check method
    if let Some(expected) = obj.get("method")
        && !check_regex_field(expected, method, &absent)
    {
        return false;
    }

    // Check path
    if let Some(expected) = obj.get("path")
        && !check_regex_field(expected, path, &absent)
    {
        return false;
    }

    // Check body
    if let Some(expected) = obj.get("body")
        && !check_regex_field(expected, body, body_json)
    {
        return false;
    }

    // Check requestFrom
    if let Some(expected) = obj.get("requestFrom")
        && !check_regex_field(expected, request_from.unwrap_or(""), &absent)
    {
        return false;
    }

    // Check ip
    if let Some(expected) = obj.get("ip")
        && !check_regex_field(expected, client_ip.unwrap_or(""), &absent)
    {
        return false;
    }

    // Form, query and headers: each key may carry several values (issues #994, #1553), and a
    // pattern list needs each of its patterns matched by some value.
    let check_value =
        |expected: &serde_json::Value, actual: &str| check_regex_field(expected, actual, &absent);
    if let Some(expected) = obj.get("form").and_then(|v| v.as_object())
        && !keyed_field_holds(
            expected,
            form.unwrap_or(&MultiValueMap::default()),
            false,
            &key_matches,
            &check_value,
        )
    {
        return false;
    }
    if let Some(expected) = obj.get("query").and_then(|v| v.as_object())
        && !keyed_field_holds(expected, query, false, &key_matches, &check_value)
    {
        return false;
    }
    if let Some(expected) = obj.get("headers").and_then(|v| v.as_object())
        && !keyed_field_holds(expected, headers, false, &key_matches, &check_value)
    {
        return false;
    }

    true
}
