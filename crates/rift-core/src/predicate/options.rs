//! Predicate options for modifying matching behavior.

use serde::{Deserialize, Serialize};

/// Options that modify predicate matching behavior.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PredicateOptions {
    /// Whether matching is case-sensitive (default: true for Rift, false for Mountebank)
    #[serde(default = "default_case_sensitive")]
    pub case_sensitive: bool,

    /// Regex pattern to strip from value before matching
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except: Option<String>,

    /// Negate the match result (NOT operator)
    #[serde(default, skip_serializing_if = "is_false")]
    pub not: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for PredicateOptions {
    fn default() -> Self {
        Self {
            case_sensitive: true, // Rift default - more performant
            except: None,
            not: false,
        }
    }
}

fn default_case_sensitive() -> bool {
    true // Rift default - more performant
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_predicate_options_default() {
        let options = PredicateOptions::default();
        assert!(options.case_sensitive); // Rift default is case-sensitive
        assert!(options.except.is_none());
        assert!(!options.not);
    }
}
