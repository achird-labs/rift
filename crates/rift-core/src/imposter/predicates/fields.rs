//! Field-oriented predicate checks (`equals`/`contains`/`startsWith`/`endsWith`/`matches`)
//! that compare a request's extracted fields against predicate values.

use super::json::compare_json_recursive;
use super::regex_cache::cached_regex;
use std::collections::HashMap;

/// Check predicate fields against request values
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_predicate_fields<F>(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &str,
    apply_except: &impl Fn(&str) -> String,
    compare: F,
    deep_equals: bool,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&HashMap<String, String>>,
    key_case_sensitive: bool,
) -> bool
where
    F: Fn(&str, &str) -> bool,
{
    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    // Helper: check a string field against expected value.
    // When expected is an object/array, parse actual as JSON and compare recursively (Mountebank compat).
    // except is applied to leaf values inside compare_json_recursive, not to raw JSON.
    // When expected is a string/primitive, compare directly with except applied.
    let check_string_field = |expected: &serde_json::Value, actual: &str| -> bool {
        match expected {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => compare_json_recursive(
                expected,
                actual,
                &compare,
                deep_equals,
                key_case_sensitive,
                &apply_except,
            ),
            _ => {
                let actual = apply_except(actual);
                match expected {
                    serde_json::Value::String(s) => compare(s, &actual),
                    _ => {
                        let expected_str = expected.to_string();
                        compare(&expected_str, &actual)
                    }
                }
            }
        }
    };

    // Check method
    if let Some(expected) = obj.get("method")
        && !check_string_field(expected, method)
    {
        return false;
    }

    // Check path
    if let Some(expected) = obj.get("path")
        && !check_string_field(expected, path)
    {
        return false;
    }

    // Check body
    if let Some(expected) = obj.get("body")
        && !check_string_field(expected, body)
    {
        return false;
    }

    // Check requestFrom (IP:port) - Mountebank compatible
    if let Some(expected) = obj.get("requestFrom") {
        let actual = request_from.unwrap_or("");
        if !check_string_field(expected, actual) {
            return false;
        }
    }

    // Check ip (just the IP address) - Mountebank compatible
    if let Some(expected) = obj.get("ip") {
        let actual = client_ip.unwrap_or("");
        if !check_string_field(expected, actual) {
            return false;
        }
    }

    // Check form fields (parsed from application/x-www-form-urlencoded) - Mountebank compatible
    if let Some(expected_form) = obj.get("form")
        && let Some(expected_obj) = expected_form.as_object()
    {
        let actual_form = form.cloned().unwrap_or_default();

        // For deepEquals, check exact match (same number of fields)
        if deep_equals && expected_obj.len() != actual_form.len() {
            return false;
        }

        for (key, expected_val) in expected_obj {
            // Find key using keyCaseSensitive option
            let actual = actual_form
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_string_field(expected_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    // Check query parameters
    if let Some(expected_query) = obj.get("query")
        && let Some(expected_obj) = expected_query.as_object()
    {
        // For deepEquals, check exact match (same number of params)
        if deep_equals && expected_obj.len() != query.len() {
            return false;
        }

        for (key, expected_val) in expected_obj {
            // Find key using keyCaseSensitive option
            let actual = query
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_string_field(expected_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    // Check headers
    if let Some(expected_headers) = obj.get("headers")
        && let Some(expected_obj) = expected_headers.as_object()
    {
        // For deepEquals, check exact match
        if deep_equals && expected_obj.len() != headers.len() {
            return false;
        }

        for (key, expected_val) in expected_obj {
            // Headers use keyCaseSensitive option
            let actual = headers
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_string_field(expected_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    true
}

/// Check predicate fields with regex matching
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_predicate_fields_regex(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &str,
    apply_except: &impl Fn(&str) -> String,
    case_sensitive: bool,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&HashMap<String, String>>,
    key_case_sensitive: bool,
) -> bool {
    // Compile-once, cached regex keyed on (pattern, case_insensitive). Returns `None` for an
    // unparseable pattern, which callers treat as "no match" — same as the previous per-request
    // `Regex::new` returning `Err`.
    let build_regex = |pattern: &str| cached_regex(pattern, !case_sensitive);

    // Helper for key comparison based on keyCaseSensitive
    let key_matches = |expected_key: &str, actual_key: &str| -> bool {
        if key_case_sensitive {
            expected_key == actual_key
        } else {
            expected_key.eq_ignore_ascii_case(actual_key)
        }
    };

    // Helper: check a field against expected value for regex matching.
    // When expected is an object/array, recurse via compare_json_recursive with regex comparator.
    // When expected is a string, build regex and match directly.
    let check_regex_field = |expected: &serde_json::Value, actual: &str| -> bool {
        match expected {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                let regex_compare = |pattern: &str, actual: &str| -> bool {
                    match build_regex(pattern) {
                        Some(re) => re.is_match(actual),
                        None => false,
                    }
                };
                compare_json_recursive(
                    expected,
                    actual,
                    &regex_compare,
                    false,
                    key_case_sensitive,
                    &apply_except,
                )
            }
            _ => {
                let pattern = match expected {
                    serde_json::Value::String(s) => s.as_str().to_string(),
                    _ => expected.to_string(),
                };
                match build_regex(&pattern) {
                    Some(re) => {
                        let actual = apply_except(actual);
                        re.is_match(&actual)
                    }
                    None => false,
                }
            }
        }
    };

    // Check method
    if let Some(expected) = obj.get("method")
        && !check_regex_field(expected, method)
    {
        return false;
    }

    // Check path
    if let Some(expected) = obj.get("path")
        && !check_regex_field(expected, path)
    {
        return false;
    }

    // Check body
    if let Some(expected) = obj.get("body")
        && !check_regex_field(expected, body)
    {
        return false;
    }

    // Check requestFrom
    if let Some(expected) = obj.get("requestFrom")
        && !check_regex_field(expected, request_from.unwrap_or(""))
    {
        return false;
    }

    // Check ip
    if let Some(expected) = obj.get("ip")
        && !check_regex_field(expected, client_ip.unwrap_or(""))
    {
        return false;
    }

    // Check form fields
    if let Some(expected_form) = obj.get("form").and_then(|v| v.as_object()) {
        let actual_form = form.cloned().unwrap_or_default();
        for (key, pattern_val) in expected_form {
            let actual = actual_form
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_regex_field(pattern_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    // Check query parameters
    if let Some(expected_query) = obj.get("query").and_then(|v| v.as_object()) {
        for (key, pattern_val) in expected_query {
            let actual = query
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_regex_field(pattern_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    // Check headers
    if let Some(expected_headers) = obj.get("headers").and_then(|v| v.as_object()) {
        for (key, pattern_val) in expected_headers {
            let actual = headers
                .iter()
                .find(|(k, _)| key_matches(key, k))
                .map(|(_, v)| v.as_str());

            match actual {
                Some(actual) => {
                    if !check_regex_field(pattern_val, actual) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    true
}
