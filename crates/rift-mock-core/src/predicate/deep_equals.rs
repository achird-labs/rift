//! Deep equality matcher for query parameters.
//!
//! Unlike regular `equals`, `deepEquals` requires an EXACT match.
//!
//! This type does **not** serve the live `deepEquals` predicate — that is
//! `PredicateOperation::DeepEquals`, matched by `imposter::predicates::fields` against the
//! collected header map. Its header half was removed in issue #1048: it had no callers, and it
//! decoded with `to_str().unwrap_or("")`, which handed any embedder calling it the exact
//! blanking that #1025 removed from the live path (a request that sent bytes would have matched
//! `{"deepEquals": {"headers": {"X-Bin": ""}}}`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Deep equality matcher for query parameters.
///
/// Unlike regular `equals`, `deepEquals` requires an EXACT match:
/// - All specified key-value pairs must be present and equal
/// - NO extra keys are allowed in the actual value
///
/// This is the Mountebank `deepEquals` predicate behavior.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DeepEquals {
    #[serde(default)]
    pub query: Option<HashMap<String, String>>,
}

/// Compiled deep equality matcher.
#[derive(Debug, Clone)]
pub struct CompiledDeepEquals {
    /// Expected query parameters
    pub query: Option<HashMap<String, String>>,
    /// Case sensitive comparison
    pub case_sensitive: bool,
}

impl CompiledDeepEquals {
    /// Compile a DeepEquals configuration.
    pub fn compile(config: &DeepEquals, case_sensitive: bool) -> Self {
        CompiledDeepEquals {
            query: config.query.clone(),
            case_sensitive,
        }
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
                params.insert(
                    crate::util::decode_or_raw(key),
                    crate::util::decode_or_raw(value),
                );
            } else if !pair.is_empty() {
                params.insert(crate::util::decode_or_raw(pair), String::new());
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_equals_query_strict() {
        let config = DeepEquals {
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

    // Issue #611: an undecodable percent-sequence must pass through raw, matching the repo's
    // decode convention, rather than blanking the value a predicate matches on.
    #[test]
    fn test_query_string_passes_through_undecodable_value() {
        let params = parse_query_string(Some("k=%FF"));
        assert_eq!(
            params.get("k"),
            Some(&"%FF".to_string()),
            "an undecodable value must pass through raw, not become empty"
        );
    }

    // Issue #614: this helper decoded only the value, leaving the key raw, so a predicate on an
    // encoded parameter *name* matched or not depending on which helper evaluated it. Mountebank
    // decodes both (Node's `querystring.parse` unescapes keys).
    #[test]
    fn test_query_string_decodes_encoded_key() {
        let params = parse_query_string(Some("first%20name=bob"));
        assert_eq!(
            params.get("first name"),
            Some(&"bob".to_string()),
            "an encoded key must be decoded, matching imposter::parse_query_string"
        );

        let undecodable = parse_query_string(Some("%FF=v"));
        assert_eq!(
            undecodable.get("%FF"),
            Some(&"v".to_string()),
            "an undecodable key must pass through raw, not collapse to an empty key"
        );
    }

    #[test]
    fn test_query_string_decodes_bare_param_key() {
        let params = parse_query_string(Some("first%20name"));
        assert_eq!(
            params.get("first name"),
            Some(&String::new()),
            "a bare param's key must be decoded on the same terms as a valued one"
        );
    }
}
