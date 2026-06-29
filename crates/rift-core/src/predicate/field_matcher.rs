//! Generic field matcher for headers and query parameters.
//!
//! This module provides a unified FieldMatcher type that can be used for both
//! HTTP headers and query parameters, eliminating duplicate code.

use super::matcher::CachedValue;
use super::options::PredicateOptions;
use super::string_matcher::{CompiledExcept, CompiledStringMatcher, StringMatcher};
use serde::{Deserialize, Serialize};

/// Generic field matching configuration.
///
/// Used for both headers and query parameters with identical behavior.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum FieldMatcher {
    /// Simple exact match (backward compatible): { name: "X-Api-Key", value: "secret" }
    Simple { name: String, value: String },

    /// OR predicate - matches if ANY of the matchers match
    Or {
        name: String,
        or: Vec<StringMatcher>,
        #[serde(flatten, default)]
        options: PredicateOptions,
    },

    /// Full predicate match with operators
    Full {
        name: String,
        #[serde(flatten)]
        matcher: StringMatcher,
        #[serde(flatten, default)]
        options: PredicateOptions,
    },
}

impl FieldMatcher {
    /// Get the field name for this matcher.
    pub fn name(&self) -> &str {
        match self {
            FieldMatcher::Simple { name, .. } => name,
            FieldMatcher::Full { name, .. } => name,
            FieldMatcher::Or { name, .. } => name,
        }
    }
}

/// Compiled single or OR field matcher.
#[derive(Debug, Clone)]
pub enum CompiledFieldMatcherInner {
    Single(CompiledStringMatcher),
    Or(Vec<CompiledStringMatcher>),
}

/// Compiled field matcher for efficient runtime evaluation.
#[derive(Debug, Clone)]
pub struct CompiledFieldMatcher {
    /// Field name (may be lowercased for headers)
    pub name: String,
    /// Compiled matcher(s)
    pub matcher: CompiledFieldMatcherInner,
    /// Predicate options
    pub case_sensitive: bool,
    /// Negate the match result (NOT operator)
    pub not: bool,
    /// Optional except pattern for stripping content before matching
    pub except: Option<CompiledExcept>,
}

impl CompiledFieldMatcher {
    /// Compile a FieldMatcher configuration.
    ///
    /// # Arguments
    /// * `config` - The field matcher configuration
    /// * `lowercase_name` - Whether to lowercase the field name (true for headers)
    pub fn compile(config: &FieldMatcher, lowercase_name: bool) -> Result<Self, regex::Error> {
        let normalize_name = |name: &str| {
            if lowercase_name {
                name.to_lowercase()
            } else {
                name.to_string()
            }
        };

        match config {
            FieldMatcher::Simple { name, value } => Ok(CompiledFieldMatcher {
                name: normalize_name(name),
                matcher: CompiledFieldMatcherInner::Single(CompiledStringMatcher::Equals(
                    CachedValue::new(value),
                )),
                case_sensitive: true,
                not: false,
                except: None,
            }),
            FieldMatcher::Or { name, or, options } => {
                let compiled: Result<Vec<_>, _> =
                    or.iter().map(CompiledStringMatcher::compile).collect();
                let except = options
                    .except
                    .as_ref()
                    .map(|p| CompiledExcept::compile(p))
                    .transpose()?;
                Ok(CompiledFieldMatcher {
                    name: normalize_name(name),
                    matcher: CompiledFieldMatcherInner::Or(compiled?),
                    case_sensitive: options.case_sensitive,
                    not: options.not,
                    except,
                })
            }
            FieldMatcher::Full {
                name,
                matcher,
                options,
            } => {
                let except = options
                    .except
                    .as_ref()
                    .map(|p| CompiledExcept::compile(p))
                    .transpose()?;
                Ok(CompiledFieldMatcher {
                    name: normalize_name(name),
                    matcher: CompiledFieldMatcherInner::Single(CompiledStringMatcher::compile(
                        matcher,
                    )?),
                    case_sensitive: options.case_sensitive,
                    not: options.not,
                    except,
                })
            }
        }
    }

    /// Check if a field value matches.
    pub fn matches(&self, value: Option<&str>) -> bool {
        let result = match &self.matcher {
            CompiledFieldMatcherInner::Single(m) => {
                m.matches_with_except(value, self.case_sensitive, self.except.as_ref())
            }
            CompiledFieldMatcherInner::Or(matchers) => matchers
                .iter()
                .any(|m| m.matches_with_except(value, self.case_sensitive, self.except.as_ref())),
        };
        if self.not {
            !result
        } else {
            result
        }
    }
}

// Type aliases for backward compatibility and clarity
pub type HeaderMatcher = FieldMatcher;
pub type QueryMatcher = FieldMatcher;
pub type CompiledHeaderMatcher = CompiledFieldMatcher;
pub type CompiledQueryMatcher = CompiledFieldMatcher;

/// Compile a header matcher (lowercases the header name).
pub fn compile_header_matcher(config: &FieldMatcher) -> Result<CompiledFieldMatcher, regex::Error> {
    CompiledFieldMatcher::compile(config, true)
}

/// Compile a query matcher (preserves the parameter name).
pub fn compile_query_matcher(config: &FieldMatcher) -> Result<CompiledFieldMatcher, regex::Error> {
    CompiledFieldMatcher::compile(config, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_matcher_simple() {
        let config = FieldMatcher::Simple {
            name: "X-Api-Key".to_string(),
            value: "secret".to_string(),
        };
        let compiled = compile_header_matcher(&config).unwrap();

        assert_eq!(compiled.name, "x-api-key"); // Lowercased
        assert!(compiled.matches(Some("secret")));
        assert!(!compiled.matches(Some("other")));
        assert!(!compiled.matches(None));
    }

    #[test]
    fn test_query_matcher_simple() {
        let config = FieldMatcher::Simple {
            name: "Page".to_string(),
            value: "1".to_string(),
        };
        let compiled = compile_query_matcher(&config).unwrap();

        assert_eq!(compiled.name, "Page"); // Not lowercased
        assert!(compiled.matches(Some("1")));
        assert!(!compiled.matches(Some("2")));
    }

    #[test]
    fn test_header_matcher_serde() {
        // Simple format (backward compatible)
        let json = r#"{"name": "X-Api-Key", "value": "secret"}"#;
        let matcher: FieldMatcher = serde_json::from_str(json).unwrap();
        assert!(matches!(matcher, FieldMatcher::Simple { .. }));

        // Full format with operators
        let json = r#"{"name": "Content-Type", "contains": "json"}"#;
        let matcher: FieldMatcher = serde_json::from_str(json).unwrap();
        assert!(matches!(matcher, FieldMatcher::Full { .. }));
    }

    #[test]
    fn test_header_matcher_with_except() {
        // Header with except - strips version numbers and matches
        let config = FieldMatcher::Full {
            name: "user-agent".to_string(),
            matcher: StringMatcher::Equals("Mozilla/ Firefox/".to_string()),
            options: PredicateOptions {
                case_sensitive: true,
                except: Some(r"\d+\.\d+".to_string()),
                not: false,
            },
        };

        let compiled = compile_header_matcher(&config).unwrap();
        assert!(compiled.matches(Some("Mozilla/5.0 Firefox/89.0")));
        assert!(compiled.matches(Some("Mozilla/6.0 Firefox/90.0")));
    }

    #[test]
    fn test_or_matcher() {
        let config = FieldMatcher::Or {
            name: "status".to_string(),
            or: vec![
                StringMatcher::Equals("active".to_string()),
                StringMatcher::Equals("pending".to_string()),
            ],
            options: PredicateOptions::default(),
        };

        let compiled = compile_query_matcher(&config).unwrap();
        assert!(compiled.matches(Some("active")));
        assert!(compiled.matches(Some("pending")));
        assert!(!compiled.matches(Some("inactive")));
    }

    #[test]
    fn test_not_matcher() {
        let config = FieldMatcher::Full {
            name: "status".to_string(),
            matcher: StringMatcher::Equals("deleted".to_string()),
            options: PredicateOptions {
                case_sensitive: true,
                except: None,
                not: true,
            },
        };

        let compiled = compile_query_matcher(&config).unwrap();
        assert!(!compiled.matches(Some("deleted")));
        assert!(compiled.matches(Some("active")));
        assert!(compiled.matches(Some("pending")));
    }
}
