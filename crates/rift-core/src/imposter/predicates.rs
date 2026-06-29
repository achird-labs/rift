//! Predicate matching logic for Mountebank-compatible stub matching.
//!
//! Supports: equals, deepEquals, contains, startsWith, endsWith, matches, exists, not, or, and
//! Also supports requestFrom, ip, and form fields.

use crate::behaviors::{extract_jsonpath, extract_xpath_with_ns};
use crate::imposter::types::{Predicate, PredicateOperation, PredicateSelector};
use std::collections::HashMap;

/// Check if a stub matches a request based on its predicates
#[allow(clippy::too_many_arguments)]
pub fn stub_matches(
    predicates: &[Predicate],
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&HashMap<String, String>>,
    imposter_port: u16,
) -> bool {
    // If no predicates, match everything
    if predicates.is_empty() {
        return true;
    }

    // All predicates must match (implicit AND)
    for predicate in predicates {
        if !predicate_matches(
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
        ) {
            return false;
        }
    }
    true
}

/// Parse query string for predicate matching, URL-decoding both keys and values
pub fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    query.map_or_else(HashMap::new, parse_query_string)
}

/// Check if a single predicate matches (Mountebank-compatible)
/// Supports: equals, deepEquals, contains, startsWith, endsWith, matches, exists, not, or, and
/// Also supports requestFrom, ip, and form fields
#[allow(clippy::too_many_arguments)]
pub fn predicate_matches(
    predicate: &Predicate,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&HashMap<String, String>>,
    imposter_port: u16,
) -> bool {
    // Get predicate options
    let case_sensitive = predicate.parameters.case_sensitive.unwrap_or(false);

    // Get keyCaseSensitive option (defaults to caseSensitive value if not specified)
    let key_case_sensitive = predicate
        .parameters
        .key_case_sensitive
        .unwrap_or(case_sensitive);

    let except_pattern = Some(predicate.parameters.except.as_str()).filter(|s| !s.is_empty());

    // Helper to apply except pattern
    let apply_except = |value: &str| -> String {
        if let Some(pattern) = except_pattern {
            if let Ok(re) = regex::Regex::new(pattern) {
                return re.replace_all(value, "").to_string();
            }
        }
        value.to_string()
    };

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
            haystack.to_lowercase().contains(&needle.to_lowercase())
        }
    };

    let str_starts_with = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.starts_with(needle)
        } else {
            haystack.to_lowercase().starts_with(&needle.to_lowercase())
        }
    };

    let str_ends_with = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.ends_with(needle)
        } else {
            haystack.to_lowercase().ends_with(&needle.to_lowercase())
        }
    };

    // Build request context for field access
    let query_map = parse_query(query);
    let body_str = body.unwrap_or("");

    // Handle jsonpath parameter - extract value from JSON body
    let extracted_body: String;
    let effective_body = match &predicate.parameters.selector {
        Some(PredicateSelector::JsonPath { selector }) => {
            extracted_body = extract_jsonpath(body_str, selector).unwrap_or_default();
            &extracted_body
        }
        Some(PredicateSelector::XPath {
            selector,
            namespaces,
        }) => {
            extracted_body =
                extract_xpath_with_ns(body_str, selector, namespaces.as_ref()).unwrap_or_default();
            &extracted_body
        }
        None => body_str,
    };

    match &predicate.operation {
        PredicateOperation::Equals(fields) => {
            check_predicate_fields(
                fields,
                method,
                path,
                &query_map,
                headers,
                effective_body,
                &apply_except,
                str_equals,
                false, // not deep equals
                request_from,
                client_ip,
                form,
                key_case_sensitive,
            )
        }
        PredicateOperation::DeepEquals(fields) => {
            check_predicate_fields(
                fields,
                method,
                path,
                &query_map,
                headers,
                effective_body,
                &apply_except,
                str_equals,
                true, // deep equals
                request_from,
                client_ip,
                form,
                key_case_sensitive,
            )
        }
        PredicateOperation::Contains(fields) => check_predicate_fields(
            fields,
            method,
            path,
            &query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_contains(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        ),
        PredicateOperation::StartsWith(fields) => check_predicate_fields(
            fields,
            method,
            path,
            &query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_starts_with(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        ),
        PredicateOperation::EndsWith(fields) => check_predicate_fields(
            fields,
            method,
            path,
            &query_map,
            headers,
            effective_body,
            &apply_except,
            |expected, actual| str_ends_with(actual, expected),
            false,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        ),
        PredicateOperation::Matches(fields) => check_predicate_fields_regex(
            fields,
            method,
            path,
            &query_map,
            headers,
            effective_body,
            &apply_except,
            case_sensitive,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        ),
        PredicateOperation::Exists(fields) => check_exists_predicate(
            fields,
            method,
            path,
            &query_map,
            headers,
            effective_body,
            request_from,
            client_ip,
            form,
            key_case_sensitive,
        ),
        PredicateOperation::Not(inner) => !predicate_matches(
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
        ),
        PredicateOperation::Or(children) => children.iter().any(|p| {
            predicate_matches(
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
            )
        }),
        PredicateOperation::And(children) => children.iter().all(|p| {
            predicate_matches(
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
            )
        }),
        PredicateOperation::Inject(inject_fn) => {
            #[cfg(feature = "javascript")]
            {
                use crate::scripting::{execute_predicate_inject, MountebankRequest};
                let query_map = parse_query(query);
                let mb_request = MountebankRequest {
                    method: method.to_string(),
                    path: path.to_string(),
                    query: query_map,
                    headers: headers.clone(),
                    body: body.map(|b| b.to_string()),
                };
                execute_predicate_inject(inject_fn, &mb_request, imposter_port)
            }
            #[cfg(not(feature = "javascript"))]
            {
                tracing::warn!(
                    "inject predicate requires the 'javascript' feature; predicate will not match"
                );
                let _ = inject_fn;
                false
            }
        }
    }
}

/// Check predicate fields against request values
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
fn check_predicate_fields<F>(
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
    if let Some(expected) = obj.get("method") {
        if !check_string_field(expected, method) {
            return false;
        }
    }

    // Check path
    if let Some(expected) = obj.get("path") {
        if !check_string_field(expected, path) {
            return false;
        }
    }

    // Check body
    if let Some(expected) = obj.get("body") {
        if !check_string_field(expected, body) {
            return false;
        }
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
    if let Some(expected_form) = obj.get("form") {
        if let Some(expected_obj) = expected_form.as_object() {
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
    }

    // Check query parameters
    if let Some(expected_query) = obj.get("query") {
        if let Some(expected_obj) = expected_query.as_object() {
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
    }

    // Check headers
    if let Some(expected_headers) = obj.get("headers") {
        if let Some(expected_obj) = expected_headers.as_object() {
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
    }

    true
}

/// Check predicate fields with regex matching
/// Supports: method, path, body, query, headers, requestFrom, ip, form
#[allow(clippy::too_many_arguments)]
fn check_predicate_fields_regex(
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
    let build_regex = |pattern: &str| -> Result<regex::Regex, regex::Error> {
        if case_sensitive {
            regex::Regex::new(pattern)
        } else {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
        }
    };

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
                        Ok(re) => re.is_match(actual),
                        Err(_) => false,
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
                    Ok(re) => {
                        let actual = apply_except(actual);
                        re.is_match(&actual)
                    }
                    Err(_) => false,
                }
            }
        }
    };

    // Check method
    if let Some(expected) = obj.get("method") {
        if !check_regex_field(expected, method) {
            return false;
        }
    }

    // Check path
    if let Some(expected) = obj.get("path") {
        if !check_regex_field(expected, path) {
            return false;
        }
    }

    // Check body
    if let Some(expected) = obj.get("body") {
        if !check_regex_field(expected, body) {
            return false;
        }
    }

    // Check requestFrom
    if let Some(expected) = obj.get("requestFrom") {
        if !check_regex_field(expected, request_from.unwrap_or("")) {
            return false;
        }
    }

    // Check ip
    if let Some(expected) = obj.get("ip") {
        if !check_regex_field(expected, client_ip.unwrap_or("")) {
            return false;
        }
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
fn check_exists_predicate(
    obj: &HashMap<String, serde_json::Value>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &str,
    request_from: Option<&str>,
    client_ip: Option<&str>,
    form: Option<&HashMap<String, String>>,
    key_case_sensitive: bool,
) -> bool {
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
    if let Some(expected) = obj.get("body") {
        if !check_exists_json_recursive(expected, body) {
            return false;
        }
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
fn compare_json_recursive<F>(
    expected: &serde_json::Value,
    actual_str: &str,
    compare: &F,
    deep_equals: bool,
    key_case_sensitive: bool,
    apply_except: &dyn Fn(&str) -> String,
) -> bool
where
    F: Fn(&str, &str) -> bool,
{
    match expected {
        serde_json::Value::Object(expected_obj) => {
            let actual_json: serde_json::Value = match serde_json::from_str(actual_str) {
                Ok(v) => v,
                Err(_) => return false,
            };

            let actual_obj = match actual_json.as_object() {
                Some(obj) => obj,
                None => return false,
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

                let actual_val = match actual_val {
                    Some(v) => v,
                    None => return false,
                };

                let actual_val_str = json_value_to_string(actual_val);
                if !compare_json_recursive(
                    expected_val,
                    &actual_val_str,
                    compare,
                    deep_equals,
                    key_case_sensitive,
                    apply_except,
                ) {
                    return false;
                }
            }
            true
        }
        serde_json::Value::Array(expected_arr) => {
            let actual_json: serde_json::Value = match serde_json::from_str(actual_str) {
                Ok(v) => v,
                Err(_) => return false,
            };

            let actual_arr = match actual_json.as_array() {
                Some(arr) => arr,
                None => return false,
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

/// Parse query string into HashMap (public helper)
/// URL-decodes both keys and values to properly handle encoded characters.
/// Bare params without `=` (e.g. `?flag`) are treated as key with empty value.
/// Duplicate keys are joined with commas (Mountebank `stringify` behavior).
pub fn parse_query_string(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let decoded_key = urlencoding::decode(key).unwrap_or_default().into_owned();
        let decoded_value = urlencoding::decode(value).unwrap_or_default().into_owned();
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
        );

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
        );

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
        );

        assert!(
            result,
            "deepEquals should match JSON bodies regardless of key order"
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
        );

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
        );

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
        );

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
        );

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
        );

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
        );

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

        assert!(predicate_matches(
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
        ));
        // Default is case-insensitive
        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_equals_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api/users"))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Equals(fields));

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_contains_body() {
        let fields: HashMap<String, serde_json::Value> =
            [("body".to_string(), json!("hello"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::Contains(fields));

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_starts_with_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("/api/"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::StartsWith(fields));

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_ends_with_path() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!(".json"))].into_iter().collect();

        let pred = make_predicate(PredicateOperation::EndsWith(fields));

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_matches_regex() {
        let fields: HashMap<String, serde_json::Value> =
            [("path".to_string(), json!("^/api/users/\\d+$"))]
                .into_iter()
                .collect();

        let pred = make_predicate(PredicateOperation::Matches(fields));

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_not_predicate() {
        let inner_fields: HashMap<String, serde_json::Value> =
            [("method".to_string(), json!("GET"))].into_iter().collect();

        let inner = make_predicate(PredicateOperation::Equals(inner_fields));
        let pred = make_predicate(PredicateOperation::Not(Box::new(inner)));

        assert!(!predicate_matches(
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
        ));
        assert!(predicate_matches(
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
        ));
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

        assert!(predicate_matches(
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
        ));
        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
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

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
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

        assert!(predicate_matches(
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
        ));
        // With caseSensitive: true, "post" should NOT match "POST"
        assert!(!predicate_matches(
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
        ));
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
        assert!(!predicate_matches(
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
        ));
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
        assert!(predicate_matches(
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
        ));
        // Body does not exist
        assert!(!predicate_matches(
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
        ));
        // Body should NOT exist (false) - empty body
        assert!(predicate_matches(
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
        ));
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

        assert!(predicate_matches(
            &pred, "GET", "/", None, &headers, None, None, None, None, 0
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_deep_equals_query_extra_params_mismatch() {
        // deepEquals on query should fail if actual has extra params
        let fields: HashMap<String, serde_json::Value> = [("query".to_string(), json!({"a": "1"}))]
            .into_iter()
            .collect();

        let pred = make_predicate(PredicateOperation::DeepEquals(fields));

        // Exact match - should pass
        assert!(predicate_matches(
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
        ));
        // Extra param - should fail for deepEquals
        assert!(!predicate_matches(
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
        ));
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

        assert!(predicate_matches(
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
        ));
        assert!(!predicate_matches(
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
        ));
    }

    #[test]
    fn test_stub_matches_empty_predicates() {
        // Empty predicates should match everything
        assert!(stub_matches(
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
        ));
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

        assert!(stub_matches(
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
        ));
        assert!(!stub_matches(
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
        ));
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
        );

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
        );

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
        );

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
        );

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
        );

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
        );

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
        );

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
        );

        assert!(
            !result,
            "exists path=false should fail when path is present"
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
        );
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
        );
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
        );
        let get_result =
            predicate_matches(&pred, "GET", "/", None, &headers, None, None, None, None, 0);
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
        );
        assert!(result, "inject predicate should access request body");
    }

    #[test]
    fn test_inject_predicate_deserializes() {
        let json = r#"{"inject": "function(request) { return true; }"}"#;
        let op: PredicateOperation = serde_json::from_str(json).expect("should deserialize inject");
        assert!(matches!(op, PredicateOperation::Inject(_)));
    }
}
