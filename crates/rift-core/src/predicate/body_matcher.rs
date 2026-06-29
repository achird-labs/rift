//! Body matching configuration and compilation.
//!
//! Supports various body matching strategies including JSON and XPath.

use super::matcher::CachedValue;
use super::string_matcher::{CompiledStringMatcher, StringMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Body matching configuration.
///
/// Supports various body matching strategies for request body content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum BodyMatcher {
    /// Exact string match
    Equals(String),

    /// String contains substring
    Contains(String),

    /// Regex pattern match
    Matches(String),

    /// JSON deep equality (for JSON bodies)
    #[serde(rename = "jsonEquals")]
    JsonEquals(serde_json::Value),

    /// JSON path expression match
    #[serde(rename = "jsonPath")]
    JsonPath {
        path: String,
        #[serde(flatten)]
        matcher: StringMatcher,
    },

    /// XPath expression match for XML bodies (Mountebank compatibility)
    #[serde(rename = "xpath")]
    XPath {
        path: String,
        #[serde(flatten)]
        matcher: StringMatcher,
    },
}

/// Compiled body matcher for efficient runtime evaluation.
#[derive(Debug, Clone)]
pub enum CompiledBodyMatcher {
    Equals(CachedValue),
    Contains(CachedValue),
    Matches(Arc<Regex>),
    JsonEquals(serde_json::Value),
    JsonPath {
        path: String,
        matcher: CompiledStringMatcher,
    },
    XPath {
        path: String,
        matcher: CompiledStringMatcher,
    },
}

impl CompiledBodyMatcher {
    /// Compile a BodyMatcher configuration.
    pub fn compile(matcher: &BodyMatcher) -> Result<Self, regex::Error> {
        match matcher {
            BodyMatcher::Equals(v) => Ok(CompiledBodyMatcher::Equals(CachedValue::new(v))),
            BodyMatcher::Contains(v) => Ok(CompiledBodyMatcher::Contains(CachedValue::new(v))),
            BodyMatcher::Matches(pattern) => {
                Ok(CompiledBodyMatcher::Matches(Arc::new(Regex::new(pattern)?)))
            }
            BodyMatcher::JsonEquals(value) => Ok(CompiledBodyMatcher::JsonEquals(value.clone())),
            BodyMatcher::JsonPath { path, matcher } => Ok(CompiledBodyMatcher::JsonPath {
                path: path.clone(),
                matcher: CompiledStringMatcher::compile(matcher)?,
            }),
            BodyMatcher::XPath { path, matcher } => Ok(CompiledBodyMatcher::XPath {
                path: path.clone(),
                matcher: CompiledStringMatcher::compile(matcher)?,
            }),
        }
    }

    /// Check if a body matches this matcher.
    pub fn matches(&self, body: &str, case_sensitive: bool) -> bool {
        match self {
            CompiledBodyMatcher::Equals(cached) => cached.equals(body, case_sensitive),
            CompiledBodyMatcher::Contains(cached) => cached.contained_in(body, case_sensitive),
            CompiledBodyMatcher::Matches(regex) => regex.is_match(body),
            CompiledBodyMatcher::JsonEquals(expected) => {
                // Parse body as JSON and compare
                match serde_json::from_str::<serde_json::Value>(body) {
                    Ok(actual) => json_deep_equals(&actual, expected, case_sensitive),
                    Err(_) => false,
                }
            }
            CompiledBodyMatcher::JsonPath { path, matcher } => {
                // Simple JSONPath implementation for common patterns
                match extract_json_path(body, path) {
                    Some(value) => matcher.matches(Some(&value), case_sensitive),
                    None => matcher.matches(None, case_sensitive),
                }
            }
            CompiledBodyMatcher::XPath { path, matcher } => {
                // XPath extraction for XML bodies
                match extract_xpath(body, path) {
                    Some(value) => matcher.matches(Some(&value), case_sensitive),
                    None => matcher.matches(None, case_sensitive),
                }
            }
        }
    }
}

/// Deep JSON equality comparison with optional case sensitivity.
fn json_deep_equals(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    case_sensitive: bool,
) -> bool {
    use serde_json::Value;

    match (actual, expected) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => {
            if case_sensitive {
                a == b
            } else {
                a.to_lowercase() == b.to_lowercase()
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| json_deep_equals(x, y, case_sensitive))
        }
        (Value::Object(a), Value::Object(b)) => {
            // All expected keys must be present and match
            b.iter().all(|(key, expected_val)| {
                a.get(key).is_some_and(|actual_val| {
                    json_deep_equals(actual_val, expected_val, case_sensitive)
                })
            })
        }
        _ => false,
    }
}

/// Extract a value from JSON using a simple JSONPath expression.
///
/// Supports:
/// - `$.field` - top-level field
/// - `$.field.nested` - nested field
/// - `$.array[0]` - array index
/// - `$.array[*].field` - all elements' field (returns first match)
pub fn extract_json_path(body: &str, path: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;

    // Remove leading $. if present
    let path = path.strip_prefix("$.").unwrap_or(path);
    let path = path.strip_prefix('$').unwrap_or(path);

    let value = navigate_json(&json, path)?;

    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        _ => Some(value.to_string()),
    }
}

/// Navigate JSON structure following a path.
fn navigate_json<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    if path.is_empty() {
        return Some(value);
    }

    // Split on first . or [
    let (segment, rest) = if let Some(bracket_pos) = path.find('[') {
        let dot_pos = path.find('.');
        match dot_pos {
            Some(d) if d < bracket_pos => {
                let (seg, rest) = path.split_at(d);
                (seg, rest.strip_prefix('.').unwrap_or(rest))
            }
            _ => {
                let (seg, rest) = path.split_at(bracket_pos);
                (seg, rest)
            }
        }
    } else if let Some(dot_pos) = path.find('.') {
        let (seg, rest) = path.split_at(dot_pos);
        (seg, rest.strip_prefix('.').unwrap_or(rest))
    } else {
        (path, "")
    };

    // Handle array index
    if segment.is_empty() && path.starts_with('[') {
        if let Some(end) = path.find(']') {
            let index_str = &path[1..end];
            let rest = path[end + 1..]
                .strip_prefix('.')
                .unwrap_or(&path[end + 1..]);

            if index_str == "*" {
                // Wildcard - return first match from array
                if let serde_json::Value::Array(arr) = value {
                    for item in arr {
                        if let Some(result) = navigate_json(item, rest) {
                            return Some(result);
                        }
                    }
                }
                return None;
            } else if let Some(stripped) = index_str.strip_prefix(':') {
                // Slice notation like [:0] - means first element (index 0)
                // In Python slicing [:0] is empty, but in Mountebank/Solo [:0] means index 0
                let slice_index = stripped.parse::<usize>().unwrap_or(0);
                let arr = value.as_array()?;
                let item = arr.get(slice_index)?;
                return navigate_json(item, rest);
            } else if let Ok(index) = index_str.parse::<usize>() {
                let arr = value.as_array()?;
                let item = arr.get(index)?;
                return navigate_json(item, rest);
            }
        }
        return None;
    }

    // Handle object field
    let obj = value.as_object()?;
    let next = obj.get(segment)?;
    navigate_json(next, rest)
}

/// Extract a value from XML using an XPath expression.
///
/// Supports common XPath patterns:
/// - `/root/element` - absolute path
/// - `//element` - descendant search
/// - `/root/element/@attribute` - attribute selection
/// - `/root/element/text()` - text content
pub fn extract_xpath(body: &str, path: &str) -> Option<String> {
    use sxd_document::parser;
    use sxd_xpath::{evaluate_xpath, Value};

    // Parse the XML document
    let package = parser::parse(body).ok()?;
    let document = package.as_document();

    // Evaluate the XPath expression
    match evaluate_xpath(&document, path) {
        Ok(value) => match value {
            Value::String(s) => Some(s),
            Value::Number(n) => {
                // Format number without unnecessary decimal places
                if n.fract() == 0.0 {
                    Some(format!("{}", n as i64))
                } else {
                    Some(n.to_string())
                }
            }
            Value::Boolean(b) => Some(b.to_string()),
            Value::Nodeset(nodes) => {
                // Return the text content of the first node
                nodes.iter().next().map(|node| node.string_value())
            }
        },
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_body_matcher_equals() {
        let matcher =
            CompiledBodyMatcher::compile(&BodyMatcher::Equals("hello world".to_string())).unwrap();

        assert!(matcher.matches("hello world", true));
        assert!(!matcher.matches("HELLO WORLD", true));
        assert!(matcher.matches("HELLO WORLD", false));
        assert!(!matcher.matches("hello", true));
    }

    #[test]
    fn test_body_matcher_contains() {
        let matcher =
            CompiledBodyMatcher::compile(&BodyMatcher::Contains("api".to_string())).unwrap();

        assert!(matcher.matches("this is an api call", true));
        assert!(!matcher.matches("this is an API call", true));
        assert!(matcher.matches("this is an API call", false));
        assert!(!matcher.matches("no match here", true));
    }

    #[test]
    fn test_body_matcher_regex() {
        let matcher =
            CompiledBodyMatcher::compile(&BodyMatcher::Matches(r"\d{3}-\d{4}".to_string()))
                .unwrap();

        assert!(matcher.matches("Call me at 123-4567", true));
        assert!(matcher.matches("Phone: 999-0000", true));
        assert!(!matcher.matches("No phone number", true));
    }

    #[test]
    fn test_body_matcher_json_equals() {
        let expected = serde_json::json!({
            "name": "John",
            "age": 30
        });
        let matcher = CompiledBodyMatcher::compile(&BodyMatcher::JsonEquals(expected)).unwrap();

        // Exact match
        assert!(matcher.matches(r#"{"name": "John", "age": 30}"#, true));

        // Order doesn't matter
        assert!(matcher.matches(r#"{"age": 30, "name": "John"}"#, true));

        // Extra fields in actual are OK (partial match)
        assert!(matcher.matches(r#"{"name": "John", "age": 30, "city": "NYC"}"#, true));

        // Missing fields fail
        assert!(!matcher.matches(r#"{"name": "John"}"#, true));

        // Wrong values fail
        assert!(!matcher.matches(r#"{"name": "Jane", "age": 30}"#, true));

        // Case insensitive string comparison
        assert!(!matcher.matches(r#"{"name": "JOHN", "age": 30}"#, true));
        assert!(matcher.matches(r#"{"name": "JOHN", "age": 30}"#, false));
    }

    #[test]
    fn test_body_matcher_json_path() {
        let matcher = CompiledBodyMatcher::compile(&BodyMatcher::JsonPath {
            path: "$.user.name".to_string(),
            matcher: StringMatcher::Equals("John".to_string()),
        })
        .unwrap();

        assert!(matcher.matches(r#"{"user": {"name": "John", "age": 30}}"#, true));
        assert!(!matcher.matches(r#"{"user": {"name": "Jane", "age": 25}}"#, true));
        assert!(!matcher.matches(r#"{"user": {"age": 30}}"#, true));
    }

    #[test]
    fn test_json_path_simple_field() {
        let body = r#"{"name": "John", "age": 30}"#;
        assert_eq!(extract_json_path(body, "$.name"), Some("John".to_string()));
        assert_eq!(extract_json_path(body, "$.age"), Some("30".to_string()));
        assert_eq!(extract_json_path(body, "$.missing"), None);
    }

    #[test]
    fn test_json_path_nested() {
        let body = r#"{"user": {"profile": {"name": "John"}}}"#;
        assert_eq!(
            extract_json_path(body, "$.user.profile.name"),
            Some("John".to_string())
        );
    }

    #[test]
    fn test_json_path_array_index() {
        let body = r#"{"users": [{"name": "Alice"}, {"name": "Bob"}]}"#;
        assert_eq!(
            extract_json_path(body, "$.users[0].name"),
            Some("Alice".to_string())
        );
        assert_eq!(
            extract_json_path(body, "$.users[1].name"),
            Some("Bob".to_string())
        );
        assert_eq!(extract_json_path(body, "$.users[2].name"), None);
    }

    #[test]
    fn test_json_path_wildcard() {
        let body = r#"{"items": [{"id": 1}, {"id": 2}, {"id": 3}]}"#;
        // Wildcard returns first match
        assert_eq!(
            extract_json_path(body, "$.items[*].id"),
            Some("1".to_string())
        );
    }

    #[test]
    fn test_json_path_slice_notation() {
        // Test [:0] slice notation used by Solo/Mountebank
        let body = r#"{"receiver":{"context":{"correlationKeys":[{"keyValue":"728839"}]}}}"#;
        assert_eq!(
            extract_json_path(body, "$.receiver.context.correlationKeys.[:0].keyValue"),
            Some("728839".to_string())
        );

        // Test with multiple items
        let body2 = r#"{"items":[{"name":"first"},{"name":"second"}]}"#;
        assert_eq!(
            extract_json_path(body2, "$.items.[:0].name"),
            Some("first".to_string())
        );
        assert_eq!(
            extract_json_path(body2, "$.items.[:1].name"),
            Some("second".to_string())
        );
    }

    #[test]
    fn test_xpath_simple_element() {
        let xml = r#"<root><name>John</name><age>30</age></root>"#;
        assert_eq!(extract_xpath(xml, "/root/name"), Some("John".to_string()));
        assert_eq!(extract_xpath(xml, "/root/age"), Some("30".to_string()));
        assert_eq!(extract_xpath(xml, "/root/missing"), None);
    }

    #[test]
    fn test_xpath_nested() {
        let xml = r#"<root><user><profile><name>Jane</name></profile></user></root>"#;
        assert_eq!(
            extract_xpath(xml, "/root/user/profile/name"),
            Some("Jane".to_string())
        );
    }

    #[test]
    fn test_xpath_attribute() {
        let xml = r#"<root><item id="123">Content</item></root>"#;
        assert_eq!(
            extract_xpath(xml, "/root/item/@id"),
            Some("123".to_string())
        );
    }

    #[test]
    fn test_xpath_descendant() {
        let xml = r#"<root><level1><level2><target>Found</target></level2></level1></root>"#;
        assert_eq!(extract_xpath(xml, "//target"), Some("Found".to_string()));
    }

    #[test]
    fn test_body_matcher_xpath() {
        let matcher = CompiledBodyMatcher::compile(&BodyMatcher::XPath {
            path: "/order/customer/name".to_string(),
            matcher: StringMatcher::Equals("Alice".to_string()),
        })
        .unwrap();

        let xml = r#"<order><customer><name>Alice</name><email>alice@example.com</email></customer></order>"#;
        assert!(matcher.matches(xml, true));

        let xml_wrong = r#"<order><customer><name>Bob</name></customer></order>"#;
        assert!(!matcher.matches(xml_wrong, true));
    }

    #[test]
    fn test_xpath_invalid_xml() {
        let invalid = "not xml at all";
        assert_eq!(extract_xpath(invalid, "/root/name"), None);
    }
}
