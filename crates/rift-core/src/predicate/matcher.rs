//! Core matching traits and helpers for predicate matchers.
//!
//! This module provides:
//! - `CachedValue` - A string value with pre-computed lowercase for efficient case-insensitive matching
//! - `StringMatchCore` - Core string matching operations used across all matcher types
//! - Helper functions for consistent case-sensitive/insensitive comparisons

use regex::Regex;
use std::sync::Arc;

/// A string value with pre-computed lowercase for efficient case-insensitive matching.
///
/// This struct eliminates the need to lowercase values on every match operation
/// by caching the lowercase version at compile time.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedValue {
    /// Original value (for case-sensitive matching)
    pub value: String,
    /// Pre-computed lowercase (for case-insensitive matching)
    pub lower: String,
}

impl CachedValue {
    /// Create a new CachedValue from a string.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let lower = value.to_lowercase();
        Self { value, lower }
    }

    /// Get the appropriate value for matching based on case sensitivity.
    #[inline]
    pub fn pattern(&self, case_sensitive: bool) -> &str {
        if case_sensitive {
            &self.value
        } else {
            &self.lower
        }
    }

    /// Check equality against a string value.
    #[inline]
    pub fn equals(&self, value: &str, case_sensitive: bool) -> bool {
        if case_sensitive {
            value == self.value
        } else {
            value.to_lowercase() == self.lower
        }
    }

    /// Check if a string value contains this pattern.
    #[inline]
    pub fn contained_in(&self, value: &str, case_sensitive: bool) -> bool {
        if case_sensitive {
            value.contains(&self.value)
        } else {
            value.to_lowercase().contains(&self.lower)
        }
    }

    /// Check if a string value starts with this pattern.
    #[inline]
    pub fn starts(&self, value: &str, case_sensitive: bool) -> bool {
        if case_sensitive {
            value.starts_with(&self.value)
        } else {
            value.to_lowercase().starts_with(&self.lower)
        }
    }

    /// Check if a string value ends with this pattern.
    #[inline]
    pub fn ends(&self, value: &str, case_sensitive: bool) -> bool {
        if case_sensitive {
            value.ends_with(&self.value)
        } else {
            value.to_lowercase().ends_with(&self.lower)
        }
    }
}

impl From<String> for CachedValue {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for CachedValue {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Core string matching operations.
///
/// This enum encapsulates the fundamental string matching operations
/// shared across all matcher types (string, path, body, field).
#[derive(Debug, Clone)]
pub enum StringMatchCore {
    /// Exact string equality
    Equals(CachedValue),
    /// String contains substring
    Contains(CachedValue),
    /// String starts with prefix
    StartsWith(CachedValue),
    /// String ends with suffix
    EndsWith(CachedValue),
    /// Regex pattern match (case sensitivity is in the pattern itself)
    Regex(Arc<Regex>),
}

impl StringMatchCore {
    /// Create an Equals matcher.
    pub fn equals(value: impl Into<String>) -> Self {
        Self::Equals(CachedValue::new(value))
    }

    /// Create a Contains matcher.
    pub fn contains(value: impl Into<String>) -> Self {
        Self::Contains(CachedValue::new(value))
    }

    /// Create a StartsWith matcher.
    pub fn starts_with(value: impl Into<String>) -> Self {
        Self::StartsWith(CachedValue::new(value))
    }

    /// Create an EndsWith matcher.
    pub fn ends_with(value: impl Into<String>) -> Self {
        Self::EndsWith(CachedValue::new(value))
    }

    /// Create a Regex matcher.
    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self::Regex(Arc::new(Regex::new(pattern)?)))
    }

    /// Check if a value matches this matcher.
    ///
    /// # Arguments
    /// * `value` - The string value to match against
    /// * `case_sensitive` - Whether to perform case-sensitive matching
    ///   (ignored for Regex - case sensitivity should be in the pattern)
    pub fn matches(&self, value: &str, case_sensitive: bool) -> bool {
        match self {
            Self::Equals(cached) => cached.equals(value, case_sensitive),
            Self::Contains(cached) => cached.contained_in(value, case_sensitive),
            Self::StartsWith(cached) => cached.starts(value, case_sensitive),
            Self::EndsWith(cached) => cached.ends(value, case_sensitive),
            Self::Regex(regex) => regex.is_match(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cached_value_new() {
        let cv = CachedValue::new("Hello World");
        assert_eq!(cv.value, "Hello World");
        assert_eq!(cv.lower, "hello world");
    }

    #[test]
    fn test_cached_value_equals() {
        let cv = CachedValue::new("Test");

        // Case sensitive
        assert!(cv.equals("Test", true));
        assert!(!cv.equals("test", true));
        assert!(!cv.equals("TEST", true));

        // Case insensitive
        assert!(cv.equals("Test", false));
        assert!(cv.equals("test", false));
        assert!(cv.equals("TEST", false));
        assert!(cv.equals("tEsT", false));
    }

    #[test]
    fn test_cached_value_contained_in() {
        let cv = CachedValue::new("api");

        // Case sensitive
        assert!(cv.contained_in("/api/users", true));
        assert!(!cv.contained_in("/API/users", true));

        // Case insensitive
        assert!(cv.contained_in("/api/users", false));
        assert!(cv.contained_in("/API/users", false));
    }

    #[test]
    fn test_cached_value_starts() {
        let cv = CachedValue::new("/api");

        // Case sensitive
        assert!(cv.starts("/api/users", true));
        assert!(!cv.starts("/API/users", true));

        // Case insensitive
        assert!(cv.starts("/api/users", false));
        assert!(cv.starts("/API/users", false));
    }

    #[test]
    fn test_cached_value_ends() {
        let cv = CachedValue::new(".json");

        // Case sensitive
        assert!(cv.ends("data.json", true));
        assert!(!cv.ends("data.JSON", true));

        // Case insensitive
        assert!(cv.ends("data.json", false));
        assert!(cv.ends("data.JSON", false));
    }

    #[test]
    fn test_string_match_core_equals() {
        let matcher = StringMatchCore::equals("test");

        assert!(matcher.matches("test", true));
        assert!(!matcher.matches("TEST", true));
        assert!(matcher.matches("TEST", false));
    }

    #[test]
    fn test_string_match_core_contains() {
        let matcher = StringMatchCore::contains("api");

        assert!(matcher.matches("/api/v1", true));
        assert!(!matcher.matches("/API/v1", true));
        assert!(matcher.matches("/API/v1", false));
    }

    #[test]
    fn test_string_match_core_starts_with() {
        let matcher = StringMatchCore::starts_with("/api");

        assert!(matcher.matches("/api/users", true));
        assert!(!matcher.matches("/API/users", true));
        assert!(matcher.matches("/API/users", false));
    }

    #[test]
    fn test_string_match_core_ends_with() {
        let matcher = StringMatchCore::ends_with(".json");

        assert!(matcher.matches("data.json", true));
        assert!(!matcher.matches("data.JSON", true));
        assert!(matcher.matches("data.JSON", false));
    }

    #[test]
    fn test_string_match_core_regex() {
        let matcher = StringMatchCore::regex(r"^/api/v\d+/").unwrap();

        assert!(matcher.matches("/api/v1/users", true));
        assert!(matcher.matches("/api/v99/items", true));
        assert!(!matcher.matches("/api/users", true));
    }

    #[test]
    fn test_cached_value_from_impls() {
        let cv1: CachedValue = "test".into();
        let cv2: CachedValue = String::from("test").into();

        assert_eq!(cv1.value, "test");
        assert_eq!(cv2.value, "test");
    }
}
