//! Logical operators for combining predicates.
//!
//! Supports Mountebank's logical operators: NOT, OR, AND.

use super::string_matcher::{CompiledStringMatcher, StringMatcher};
use serde::{Deserialize, Serialize};

/// Logical predicate for combining multiple string matchers.
///
/// Supports Mountebank's logical operators: not, or, and.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum LogicalMatcher {
    /// Negates the inner matcher
    Not(Box<LogicalMatcher>),

    /// Matches if ANY of the inner matchers match
    Or(Vec<LogicalMatcher>),

    /// Matches if ALL of the inner matchers match
    And(Vec<LogicalMatcher>),

    /// A leaf string matcher
    #[serde(untagged)]
    Leaf(StringMatcher),
}

impl Default for LogicalMatcher {
    fn default() -> Self {
        LogicalMatcher::Leaf(StringMatcher::Exists(true))
    }
}

/// Compiled logical matcher for efficient runtime evaluation.
#[derive(Debug, Clone)]
pub enum CompiledLogicalMatcher {
    Not(Box<CompiledLogicalMatcher>),
    Or(Vec<CompiledLogicalMatcher>),
    And(Vec<CompiledLogicalMatcher>),
    Leaf(CompiledStringMatcher),
}

impl CompiledLogicalMatcher {
    /// Compile a LogicalMatcher configuration.
    pub fn compile(matcher: &LogicalMatcher) -> Result<Self, regex::Error> {
        match matcher {
            LogicalMatcher::Not(inner) => {
                Ok(CompiledLogicalMatcher::Not(Box::new(Self::compile(inner)?)))
            }
            LogicalMatcher::Or(matchers) => {
                let compiled: Result<Vec<_>, _> = matchers.iter().map(Self::compile).collect();
                Ok(CompiledLogicalMatcher::Or(compiled?))
            }
            LogicalMatcher::And(matchers) => {
                let compiled: Result<Vec<_>, _> = matchers.iter().map(Self::compile).collect();
                Ok(CompiledLogicalMatcher::And(compiled?))
            }
            LogicalMatcher::Leaf(string_matcher) => Ok(CompiledLogicalMatcher::Leaf(
                CompiledStringMatcher::compile(string_matcher)?,
            )),
        }
    }

    /// Check if a value matches this logical matcher.
    pub fn matches(&self, value: Option<&str>, case_sensitive: bool) -> bool {
        match self {
            CompiledLogicalMatcher::Not(inner) => !inner.matches(value, case_sensitive),
            CompiledLogicalMatcher::Or(matchers) => {
                matchers.iter().any(|m| m.matches(value, case_sensitive))
            }
            CompiledLogicalMatcher::And(matchers) => {
                matchers.iter().all(|m| m.matches(value, case_sensitive))
            }
            CompiledLogicalMatcher::Leaf(matcher) => matcher.matches(value, case_sensitive),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logical_not() {
        let matcher = CompiledLogicalMatcher::compile(&LogicalMatcher::Not(Box::new(
            LogicalMatcher::Leaf(StringMatcher::Equals("test".to_string())),
        )))
        .unwrap();

        assert!(!matcher.matches(Some("test"), true)); // NOT equals "test"
        assert!(matcher.matches(Some("other"), true)); // NOT equals "other" -> true
        assert!(matcher.matches(None, true)); // NOT exists -> true
    }

    #[test]
    fn test_logical_or() {
        let matcher = CompiledLogicalMatcher::compile(&LogicalMatcher::Or(vec![
            LogicalMatcher::Leaf(StringMatcher::Equals("foo".to_string())),
            LogicalMatcher::Leaf(StringMatcher::Equals("bar".to_string())),
            LogicalMatcher::Leaf(StringMatcher::Equals("baz".to_string())),
        ]))
        .unwrap();

        assert!(matcher.matches(Some("foo"), true));
        assert!(matcher.matches(Some("bar"), true));
        assert!(matcher.matches(Some("baz"), true));
        assert!(!matcher.matches(Some("qux"), true));
        assert!(!matcher.matches(None, true));
    }

    #[test]
    fn test_logical_and() {
        let matcher = CompiledLogicalMatcher::compile(&LogicalMatcher::And(vec![
            LogicalMatcher::Leaf(StringMatcher::Contains("api".to_string())),
            LogicalMatcher::Leaf(StringMatcher::StartsWith("/".to_string())),
        ]))
        .unwrap();

        assert!(matcher.matches(Some("/api/v1"), true));
        assert!(matcher.matches(Some("/my-api"), true));
        assert!(!matcher.matches(Some("api/v1"), true)); // Doesn't start with /
        assert!(!matcher.matches(Some("/users"), true)); // Doesn't contain api
    }

    #[test]
    fn test_logical_nested() {
        // NOT (foo OR bar) - should match anything except "foo" or "bar"
        let matcher = CompiledLogicalMatcher::compile(&LogicalMatcher::Not(Box::new(
            LogicalMatcher::Or(vec![
                LogicalMatcher::Leaf(StringMatcher::Equals("foo".to_string())),
                LogicalMatcher::Leaf(StringMatcher::Equals("bar".to_string())),
            ]),
        )))
        .unwrap();

        assert!(!matcher.matches(Some("foo"), true));
        assert!(!matcher.matches(Some("bar"), true));
        assert!(matcher.matches(Some("baz"), true));
        assert!(matcher.matches(Some("anything"), true));
    }
}
