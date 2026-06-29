//! Extraction methods: regex, JSONPath, XPath.

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

/// Regex matching options (Mountebank-compatible)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegexOptions {
    /// Case-insensitive matching
    #[serde(default)]
    pub ignore_case: bool,
    /// Multiline mode (`^`/`$` match line boundaries)
    #[serde(default)]
    pub multiline: bool,
}

/// Method for extracting values from source
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum ExtractionMethod {
    /// Regular expression with capture groups
    Regex {
        selector: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<RegexOptions>,
    },
    /// JSONPath expression
    #[serde(rename = "jsonpath")]
    JsonPath { selector: String },
    /// XPath expression for XML
    #[serde(rename = "xpath")]
    XPath { selector: String },
}

impl ExtractionMethod {
    /// Apply extraction to a value
    pub fn extract(&self, value: &str) -> Option<String> {
        match self {
            ExtractionMethod::Regex { selector, options } => {
                let opts = options.as_ref();
                let re = RegexBuilder::new(selector)
                    .case_insensitive(opts.is_some_and(|o| o.ignore_case))
                    .multi_line(opts.is_some_and(|o| o.multiline))
                    .build()
                    .ok()?;
                if let Some(caps) = re.captures(value) {
                    // Return first capture group if exists, otherwise full match
                    caps.get(1)
                        .or_else(|| caps.get(0))
                        .map(|m| m.as_str().to_string())
                } else {
                    None
                }
            }
            ExtractionMethod::JsonPath { selector } => extract_jsonpath(value, selector),
            ExtractionMethod::XPath { selector } => extract_xpath(value, selector),
        }
    }
}

/// Extract value using JSONPath (RFC 9535 compliant via serde_json_path)
/// Used by copy behaviors and predicate jsonpath parameter.
/// Supports the full JSONPath spec: wildcards, descendant segments, filters,
/// negative indices, selector sequences, bracket notation, etc.
pub fn extract_jsonpath(json_str: &str, path: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let json_path = serde_json_path::JsonPath::parse(path).ok()?;
    let node_list = json_path.query(&json);

    // Return the first matched node as a string
    let first = node_list.first()?;
    match first {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        _ => Some(first.to_string()),
    }
}

/// Extract value using XPath, optionally with namespace prefix bindings.
/// Used by copy behaviors and predicate xpath parameter.
pub fn extract_xpath(xml_str: &str, path: &str) -> Option<String> {
    extract_xpath_with_ns(xml_str, path, None)
}

/// Extract value using XPath with optional namespace prefix→URI map.
pub fn extract_xpath_with_ns(
    xml_str: &str,
    path: &str,
    ns: Option<&std::collections::HashMap<String, String>>,
) -> Option<String> {
    use sxd_document::parser;
    use sxd_xpath::{Context, Factory, Value};

    let package = parser::parse(xml_str).ok()?;
    let document = package.as_document();

    let factory = Factory::new();
    let xpath = factory.build(path).ok()??;

    let mut context = Context::new();
    if let Some(namespaces) = ns {
        for (prefix, uri) in namespaces {
            context.set_namespace(prefix, uri);
        }
    }

    let root = document.root();
    match xpath.evaluate(&context, root) {
        Ok(Value::String(s)) => Some(s),
        Ok(Value::Number(n)) => Some(n.to_string()),
        Ok(Value::Boolean(b)) => Some(b.to_string()),
        Ok(Value::Nodeset(nodes)) => nodes.iter().next().map(|n| n.string_value()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extraction_regex() {
        let method = ExtractionMethod::Regex {
            selector: r"/users/(\d+)".to_string(),
            options: None,
        };
        assert_eq!(method.extract("/users/123"), Some("123".to_string()));
        assert_eq!(method.extract("/posts/456"), None);
    }

    #[test]
    fn test_extraction_regex_full_match() {
        let method = ExtractionMethod::Regex {
            selector: r".*".to_string(),
            options: None,
        };
        assert_eq!(
            method.extract("hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extraction_jsonpath() {
        let method = ExtractionMethod::JsonPath {
            selector: "$.user.name".to_string(),
        };
        let json = r#"{"user": {"name": "Alice", "age": 30}}"#;
        assert_eq!(method.extract(json), Some("Alice".to_string()));
    }

    #[test]
    fn test_extraction_jsonpath_array() {
        let method = ExtractionMethod::JsonPath {
            selector: "$.items[0]".to_string(),
        };
        let json = r#"{"items": ["first", "second"]}"#;
        assert_eq!(method.extract(json), Some("first".to_string()));
    }

    // =========================================================================
    // Issue #78: JSONPath RFC 9535 compliance tests
    // =========================================================================

    // Test data matching the RFC 9535 examples section
    const STORE_JSON: &str = r#"{
        "store": {
            "book": [
                {
                    "category": "reference",
                    "author": "Nigel Rees",
                    "title": "Sayings of the Century",
                    "price": 8.95
                },
                {
                    "category": "fiction",
                    "author": "Evelyn Waugh",
                    "title": "Sword of Honour",
                    "price": 12.99
                },
                {
                    "category": "fiction",
                    "author": "Herman Melville",
                    "title": "Moby Dick",
                    "isbn": "0-553-21311-3",
                    "price": 8.99
                },
                {
                    "category": "fiction",
                    "author": "J. R. R. Tolkien",
                    "title": "The Lord of the Rings",
                    "isbn": "0-395-19395-8",
                    "price": 22.99
                }
            ],
            "bicycle": {
                "color": "red",
                "price": 399.99
            }
        }
    }"#;

    #[test]
    fn test_jsonpath_wildcard_selector() {
        // $.store.book[*].author → all authors
        let result = extract_jsonpath(STORE_JSON, "$.store.book[*].author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Nigel Rees");
    }

    #[test]
    fn test_jsonpath_descendant_author() {
        // $..author → all authors (descendant segment)
        let result = extract_jsonpath(STORE_JSON, "$..author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Nigel Rees");
    }

    #[test]
    fn test_jsonpath_descendant_price() {
        // $.store..price → prices of everything in the store
        let result = extract_jsonpath(STORE_JSON, "$.store..price");
        assert!(result.is_some());
        // serde_json uses BTreeMap (alphabetical key ordering), so "bicycle" comes before "book"
        assert_eq!(result.unwrap(), "399.99");
    }

    #[test]
    fn test_jsonpath_array_index() {
        // $..book[2] → the third book
        let result = extract_jsonpath(STORE_JSON, "$..book[2].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Moby Dick");
    }

    #[test]
    fn test_jsonpath_array_index_author() {
        // $..book[2].author → the third book's author
        let result = extract_jsonpath(STORE_JSON, "$..book[2].author");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Herman Melville");
    }

    #[test]
    fn test_jsonpath_missing_field() {
        // $..book[2].publisher → empty (third book has no publisher)
        let result = extract_jsonpath(STORE_JSON, "$..book[2].publisher");
        assert!(result.is_none());
    }

    #[test]
    fn test_jsonpath_negative_index() {
        // $..book[-1] → the last book
        let result = extract_jsonpath(STORE_JSON, "$..book[-1].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "The Lord of the Rings");
    }

    #[test]
    fn test_jsonpath_slice_first_two() {
        // $..book[:2] → the first two books (slice notation)
        let result = extract_jsonpath(STORE_JSON, "$..book[:2]");
        assert!(result.is_some());
    }

    #[test]
    fn test_jsonpath_filter_isbn() {
        // $..book[?@.isbn] → all books with an ISBN
        let result = extract_jsonpath(STORE_JSON, "$..book[?@.isbn].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Moby Dick");
    }

    #[test]
    fn test_jsonpath_filter_price() {
        // $..book[?@.price<10] → all books cheaper than 10
        let result = extract_jsonpath(STORE_JSON, "$..book[?@.price<10].title");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Sayings of the Century");
    }

    #[test]
    fn test_jsonpath_bracket_notation() {
        // $['store']['bicycle']['color'] → bracket notation for string index
        let result = extract_jsonpath(STORE_JSON, "$['store']['bicycle']['color']");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "red");
    }

    #[test]
    fn test_jsonpath_store_wildcard() {
        // $.store.* → all things in the store
        let result = extract_jsonpath(STORE_JSON, "$.store.*");
        assert!(result.is_some());
    }

    #[test]
    fn test_jsonpath_basic_still_works() {
        // Ensure basic paths like $.field and $.nested.field still work
        let json = r#"{"user": {"name": "Alice", "age": 30}}"#;
        assert_eq!(
            extract_jsonpath(json, "$.user.name"),
            Some("Alice".to_string())
        );
        assert_eq!(extract_jsonpath(json, "$.user.age"), Some("30".to_string()));

        let json = r#"{"items": ["first", "second"]}"#;
        assert_eq!(
            extract_jsonpath(json, "$.items[0]"),
            Some("first".to_string())
        );
        assert_eq!(
            extract_jsonpath(json, "$.items[1]"),
            Some("second".to_string())
        );
    }

    #[test]
    fn test_extraction_regex_ignore_case() {
        let method = ExtractionMethod::Regex {
            selector: "hello".to_string(),
            options: Some(RegexOptions {
                ignore_case: true,
                multiline: false,
            }),
        };
        assert_eq!(method.extract("HELLO world"), Some("HELLO".to_string()));
        assert_eq!(method.extract("nope"), None);
    }

    #[test]
    fn test_extraction_regex_multiline() {
        let method = ExtractionMethod::Regex {
            selector: r"^line2".to_string(),
            options: Some(RegexOptions {
                ignore_case: false,
                multiline: true,
            }),
        };
        assert_eq!(
            method.extract("line1\nline2\nline3"),
            Some("line2".to_string())
        );
    }

    #[test]
    fn test_extraction_regex_options_serde() {
        let json = r#"{"method": "regex", "selector": ".*", "options": {"ignoreCase": true, "multiline": false}}"#;
        let method: ExtractionMethod = serde_json::from_str(json).unwrap();
        match method {
            ExtractionMethod::Regex {
                options: Some(opts),
                ..
            } => {
                assert!(opts.ignore_case);
                assert!(!opts.multiline);
            }
            _ => panic!("Expected Regex with options"),
        }
    }

    #[test]
    fn test_extract_xpath_without_namespaces() {
        let xml = r#"<root><child>value</child></root>"#;
        assert_eq!(extract_xpath(xml, "//child"), Some("value".to_string()));
    }

    #[test]
    fn test_extract_xpath_with_ns_map() {
        let xml = r#"<ns:root xmlns:ns="http://example.com/ns"><ns:item>hello</ns:item></ns:root>"#;
        let mut ns = std::collections::HashMap::new();
        ns.insert("ns".to_string(), "http://example.com/ns".to_string());
        let result = extract_xpath_with_ns(xml, "//ns:item", Some(&ns));
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn test_extract_xpath_with_multiple_ns_bindings() {
        let xml = r#"<a:root xmlns:a="http://a.com" xmlns:b="http://b.com"><a:x><b:y>found</b:y></a:x></a:root>"#;
        let mut ns = std::collections::HashMap::new();
        ns.insert("a".to_string(), "http://a.com".to_string());
        ns.insert("b".to_string(), "http://b.com".to_string());
        let result = extract_xpath_with_ns(xml, "//a:x/b:y", Some(&ns));
        assert_eq!(result, Some("found".to_string()));
    }
}
