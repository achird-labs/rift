//! String matching types and compilation.
//!
//! This module provides the core string matching capabilities used throughout
//! the predicate system. It supports all Mountebank string matching operations.

use super::matcher::CachedValue;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// String matching operator for comparing string values.
///
/// Supports all Mountebank string matching operations.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum StringMatcher {
    /// Exact string equality
    #[serde(rename = "equals")]
    Equals(String),

    /// String contains substring
    #[serde(rename = "contains")]
    Contains(String),

    /// String starts with prefix
    #[serde(rename = "startsWith")]
    StartsWith(String),

    /// String ends with suffix
    #[serde(rename = "endsWith")]
    EndsWith(String),

    /// Regex pattern match
    #[serde(rename = "matches")]
    Matches(String),

    /// Field existence check (value is whether field should exist)
    #[serde(rename = "exists")]
    Exists(bool),
}

impl Default for StringMatcher {
    fn default() -> Self {
        StringMatcher::Exists(true)
    }
}

/// Compiled string matcher for efficient runtime evaluation.
#[derive(Debug, Clone)]
pub enum CompiledStringMatcher {
    Equals(CachedValue),
    Contains(CachedValue),
    StartsWith(CachedValue),
    EndsWith(CachedValue),
    Matches(Arc<Regex>),
    Exists(bool),
}

impl CompiledStringMatcher {
    /// Compile a StringMatcher into an efficient runtime form.
    pub fn compile(matcher: &StringMatcher) -> Result<Self, regex::Error> {
        match matcher {
            StringMatcher::Equals(v) => Ok(CompiledStringMatcher::Equals(CachedValue::new(v))),
            StringMatcher::Contains(v) => Ok(CompiledStringMatcher::Contains(CachedValue::new(v))),
            StringMatcher::StartsWith(v) => {
                Ok(CompiledStringMatcher::StartsWith(CachedValue::new(v)))
            }
            StringMatcher::EndsWith(v) => Ok(CompiledStringMatcher::EndsWith(CachedValue::new(v))),
            StringMatcher::Matches(pattern) => {
                let regex = Regex::new(pattern)?;
                Ok(CompiledStringMatcher::Matches(Arc::new(regex)))
            }
            StringMatcher::Exists(exists) => Ok(CompiledStringMatcher::Exists(*exists)),
        }
    }

    /// Check if a value matches this matcher.
    ///
    /// # Arguments
    /// * `value` - The value to match against (None if field doesn't exist)
    /// * `case_sensitive` - Whether to perform case-sensitive matching
    pub fn matches(&self, value: Option<&str>, case_sensitive: bool) -> bool {
        match (self, value) {
            // Exists check
            (CompiledStringMatcher::Exists(should_exist), v) => {
                let does_exist = v.is_some();
                *should_exist == does_exist
            }

            // For all other matchers, value must exist
            (_, None) => false,

            (CompiledStringMatcher::Equals(cached), Some(v)) => cached.equals(v, case_sensitive),

            (CompiledStringMatcher::Contains(cached), Some(v)) => {
                cached.contained_in(v, case_sensitive)
            }

            (CompiledStringMatcher::StartsWith(cached), Some(v)) => {
                cached.starts(v, case_sensitive)
            }

            (CompiledStringMatcher::EndsWith(cached), Some(v)) => cached.ends(v, case_sensitive),

            (CompiledStringMatcher::Matches(regex), Some(v)) => {
                // Regex matching - case sensitivity should be in the pattern itself
                regex.is_match(v)
            }
        }
    }

    /// Check if a value matches this matcher, applying an optional except pattern first.
    ///
    /// The except pattern strips matching content from the value before comparison.
    pub fn matches_with_except(
        &self,
        value: Option<&str>,
        case_sensitive: bool,
        except: Option<&CompiledExcept>,
    ) -> bool {
        // Apply except pattern if present
        let processed_value = match (value, except) {
            (Some(v), Some(exc)) => Some(exc.apply(v)),
            (Some(v), None) => Some(v.to_string()),
            (None, _) => None,
        };

        self.matches(processed_value.as_deref(), case_sensitive)
    }
}

/// Compiled except regex for stripping patterns before matching.
#[derive(Debug, Clone)]
pub struct CompiledExcept {
    pub regex: Arc<Regex>,
}

impl CompiledExcept {
    /// Compile an except regex pattern.
    pub fn compile(pattern: &str) -> Result<Self, regex::Error> {
        Ok(CompiledExcept {
            regex: Arc::new(Regex::new(pattern)?),
        })
    }

    /// Apply the except pattern, stripping matching content from the value.
    pub fn apply(&self, value: &str) -> String {
        self.regex.replace_all(value, "").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_matcher_equals() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::Equals("test".to_string())).unwrap();

        assert!(matcher.matches(Some("test"), true));
        assert!(!matcher.matches(Some("TEST"), true));
        assert!(matcher.matches(Some("TEST"), false));
        assert!(!matcher.matches(Some("other"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_string_matcher_contains() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::Contains("api".to_string())).unwrap();

        assert!(matcher.matches(Some("/api/v1"), true));
        assert!(matcher.matches(Some("my-api-service"), true));
        assert!(!matcher.matches(Some("/API/v1"), true));
        assert!(matcher.matches(Some("/API/v1"), false));
        assert!(!matcher.matches(Some("other"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_string_matcher_starts_with() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::StartsWith("/api".to_string())).unwrap();

        assert!(matcher.matches(Some("/api/v1"), true));
        assert!(matcher.matches(Some("/api"), true));
        assert!(!matcher.matches(Some("/API/v1"), true));
        assert!(matcher.matches(Some("/API/v1"), false));
        assert!(!matcher.matches(Some("other/api"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_string_matcher_ends_with() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::EndsWith(".json".to_string())).unwrap();

        assert!(matcher.matches(Some("/data.json"), true));
        assert!(matcher.matches(Some(".json"), true));
        assert!(!matcher.matches(Some("/data.JSON"), true));
        assert!(matcher.matches(Some("/data.JSON"), false));
        assert!(!matcher.matches(Some("/data.xml"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_string_matcher_regex() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::Matches(r"^/api/v\d+/".to_string()))
                .unwrap();

        assert!(matcher.matches(Some("/api/v1/users"), true));
        assert!(matcher.matches(Some("/api/v99/items"), true));
        assert!(!matcher.matches(Some("/api/users"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_string_matcher_exists() {
        let exists_true = CompiledStringMatcher::compile(&StringMatcher::Exists(true)).unwrap();
        let exists_false = CompiledStringMatcher::compile(&StringMatcher::Exists(false)).unwrap();

        assert!(exists_true.matches(Some("any value"), true));
        assert!(exists_true.matches(Some(""), true));
        assert!(!exists_true.matches(None, true));

        assert!(!exists_false.matches(Some("any value"), true));
        assert!(exists_false.matches(None, true));
    }

    #[test]
    fn test_string_matcher_serde() {
        // Test equals
        let json = r#"{"equals": "test"}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::Equals("test".to_string()));

        // Test contains
        let json = r#"{"contains": "api"}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::Contains("api".to_string()));

        // Test startsWith
        let json = r#"{"startsWith": "/api"}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::StartsWith("/api".to_string()));

        // Test endsWith
        let json = r#"{"endsWith": ".json"}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::EndsWith(".json".to_string()));

        // Test matches (regex)
        let json = r#"{"matches": "^/api/v\\d+"}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::Matches(r"^/api/v\d+".to_string()));

        // Test exists
        let json = r#"{"exists": true}"#;
        let matcher: StringMatcher = serde_json::from_str(json).unwrap();
        assert_eq!(matcher, StringMatcher::Exists(true));
    }

    #[test]
    fn test_except_parameter() {
        let except = CompiledExcept::compile(r"\d+").unwrap();

        // Strips all digits
        assert_eq!(except.apply("abc123def456"), "abcdef");
        assert_eq!(except.apply("12345"), "");
        assert_eq!(except.apply("no-digits-here"), "no-digits-here");
    }

    #[test]
    fn test_string_matcher_with_except() {
        let matcher =
            CompiledStringMatcher::compile(&StringMatcher::Equals("Hello World".to_string()))
                .unwrap();
        let except = CompiledExcept::compile(r"\d+").unwrap();

        // Without except - doesn't match
        assert!(!matcher.matches(Some("Hello123 World456"), true));

        // With except - strips digits and matches
        assert!(matcher.matches_with_except(Some("Hello123 World456"), true, Some(&except)));
    }
}
