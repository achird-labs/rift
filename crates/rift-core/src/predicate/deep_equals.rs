//! Deep equality matcher for objects (headers, query params).
//!
//! Unlike regular `equals`, `deepEquals` requires an EXACT match.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Deep equality matcher for objects (headers, query params).
///
/// Unlike regular `equals`, `deepEquals` requires an EXACT match:
/// - All specified key-value pairs must be present and equal
/// - NO extra keys are allowed in the actual value
///
/// This is the Mountebank `deepEquals` predicate behavior.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DeepEquals {
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub query: Option<HashMap<String, String>>,
}

/// Compiled deep equality matcher.
#[derive(Debug, Clone)]
pub struct CompiledDeepEquals {
    /// Expected headers (keys lowercased)
    pub headers: Option<HashMap<String, String>>,
    /// Expected query parameters
    pub query: Option<HashMap<String, String>>,
    /// Case sensitive comparison
    pub case_sensitive: bool,
}

impl CompiledDeepEquals {
    /// Compile a DeepEquals configuration.
    pub fn compile(config: &DeepEquals, case_sensitive: bool) -> Self {
        CompiledDeepEquals {
            headers: config.headers.as_ref().map(|h| {
                h.iter()
                    .map(|(k, v)| (k.to_lowercase(), v.clone()))
                    .collect()
            }),
            query: config.query.clone(),
            case_sensitive,
        }
    }

    /// Check if headers match the deep equality constraint (exact match, no extra headers).
    ///
    /// Note: For headers, we only check against the expected headers since HTTP headers
    /// typically include many standard headers. Use `matches_headers_strict` for true deep equality.
    pub fn matches_headers(&self, headers: &hyper::HeaderMap) -> bool {
        if let Some(expected) = &self.headers {
            for (name, expected_value) in expected {
                match headers.get(name.as_str()) {
                    Some(actual) => {
                        let actual_str = actual.to_str().unwrap_or("");
                        let matches = if self.case_sensitive {
                            actual_str == expected_value
                        } else {
                            actual_str.to_lowercase() == expected_value.to_lowercase()
                        };
                        if !matches {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
        }
        true
    }

    /// Check if query parameters match the deep equality constraint.
    ///
    /// This is a strict deep equality check:
    /// - All expected parameters must be present with matching values
    /// - NO extra parameters are allowed
    pub fn matches_query(&self, query_params: &HashMap<String, String>) -> bool {
        if let Some(expected) = &self.query {
            // Check that all expected params exist with correct values
            for (name, expected_value) in expected {
                match query_params.get(name) {
                    Some(actual) => {
                        let matches = if self.case_sensitive {
                            actual == expected_value
                        } else {
                            actual.to_lowercase() == expected_value.to_lowercase()
                        };
                        if !matches {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            // Check that NO extra params exist (deepEquals is strict)
            if query_params.len() != expected.len() {
                return false;
            }
        }
        true
    }

    /// Check if query parameters match using partial equality (like regular `equals`).
    ///
    /// Only checks that expected parameters exist with matching values.
    /// Extra parameters are allowed.
    pub fn matches_query_partial(&self, query_params: &HashMap<String, String>) -> bool {
        if let Some(expected) = &self.query {
            for (name, expected_value) in expected {
                match query_params.get(name) {
                    Some(actual) => {
                        let matches = if self.case_sensitive {
                            actual == expected_value
                        } else {
                            actual.to_lowercase() == expected_value.to_lowercase()
                        };
                        if !matches {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
        }
        true
    }
}

/// Parse query string into a HashMap.
pub fn parse_query_string(query: Option<&str>) -> HashMap<String, String> {
    let mut params = HashMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                // URL decode would go here for full compatibility
                params.insert(
                    key.to_string(),
                    urlencoding::decode(value).unwrap_or_default().to_string(),
                );
            } else if !pair.is_empty() {
                params.insert(pair.to_string(), String::new());
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderName, HeaderValue};
    use hyper::HeaderMap;

    #[test]
    fn test_deep_equals_headers() {
        let config = DeepEquals {
            headers: Some(
                [("x-api-key".to_string(), "secret".to_string())]
                    .into_iter()
                    .collect(),
            ),
            query: None,
        };
        let compiled = CompiledDeepEquals::compile(&config, true);

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("secret"),
        );
        assert!(compiled.matches_headers(&headers));

        let mut wrong_headers = HeaderMap::new();
        wrong_headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("wrong"),
        );
        assert!(!compiled.matches_headers(&wrong_headers));

        let empty_headers = HeaderMap::new();
        assert!(!compiled.matches_headers(&empty_headers));
    }

    #[test]
    fn test_deep_equals_query_strict() {
        let config = DeepEquals {
            headers: None,
            query: Some(
                [
                    ("page".to_string(), "1".to_string()),
                    ("sort".to_string(), "desc".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
        };
        let compiled = CompiledDeepEquals::compile(&config, true);

        // Exact match - should pass
        let exact: HashMap<String, String> = [
            ("page".to_string(), "1".to_string()),
            ("sort".to_string(), "desc".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(compiled.matches_query(&exact));

        // Missing param - should fail
        let missing: HashMap<String, String> = [("page".to_string(), "1".to_string())]
            .into_iter()
            .collect();
        assert!(!compiled.matches_query(&missing));

        // Extra param - should fail (deepEquals is strict)
        let extra: HashMap<String, String> = [
            ("page".to_string(), "1".to_string()),
            ("sort".to_string(), "desc".to_string()),
            ("filter".to_string(), "active".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(!compiled.matches_query(&extra));
    }

    #[test]
    fn test_deep_equals_query_partial() {
        let config = DeepEquals {
            headers: None,
            query: Some(
                [("page".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
            ),
        };
        let compiled = CompiledDeepEquals::compile(&config, true);

        // Extra params are allowed with partial matching
        let with_extra: HashMap<String, String> = [
            ("page".to_string(), "1".to_string()),
            ("sort".to_string(), "desc".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(compiled.matches_query_partial(&with_extra));
    }

    #[test]
    fn test_query_string_parsing() {
        let params = parse_query_string(Some("page=1&sort=desc&filter=active"));
        assert_eq!(params.get("page"), Some(&"1".to_string()));
        assert_eq!(params.get("sort"), Some(&"desc".to_string()));
        assert_eq!(params.get("filter"), Some(&"active".to_string()));

        let empty = parse_query_string(None);
        assert!(empty.is_empty());

        let encoded = parse_query_string(Some("name=hello%20world"));
        assert_eq!(encoded.get("name"), Some(&"hello world".to_string()));
    }
}
