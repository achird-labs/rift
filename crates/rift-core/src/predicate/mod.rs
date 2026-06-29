//! Mountebank-compatible predicate system for request matching.
//!
//! This module provides a comprehensive predicate system that supports all Mountebank
//! predicate operators (equals, contains, startsWith, endsWith, matches, exists, deepEquals)
//! with logical operators (AND, OR, NOT) and predicate parameters (caseSensitive, except).
//!
//! # Design Goals
//!
//! 1. **Mountebank Compatibility**: Support all Mountebank predicate types
//! 2. **Performance**: Pre-compile regexes, efficient string matching
//! 3. **Runtime Updates**: Designed for hot-reload with imposter support
//! 4. **Backward Compatibility**: Existing Rift configs continue to work
//!
//! # Module Structure
//!
//! - `matcher` - Core matching traits and helpers (CachedValue, StringMatchCore)
//! - `string_matcher` - Core string matching (equals, contains, startsWith, etc.)
//! - `options` - Predicate options (caseSensitive, except, not)
//! - `field_matcher` - Generic field matcher for headers and query parameters
//! - `path_matcher` - Path matching with backward compatibility
//! - `body_matcher` - Body matching (JSON, XPath, regex)
//! - `logical` - Logical operators (NOT, OR, AND)
//! - `deep_equals` - Deep equality for objects
//! - `request` - Unified request predicate

// Allow dead code while predicate system is being fully integrated
#![allow(dead_code)]

mod body_matcher;
mod deep_equals;
mod field_matcher;
mod logical;
mod matcher;
mod options;
mod path_matcher;
mod request;
mod string_matcher;

// Re-export all public types for external consumers
// Some are not yet used internally but are part of the public API
#[allow(unused_imports)]
pub use body_matcher::{extract_json_path, extract_xpath, BodyMatcher, CompiledBodyMatcher};
#[allow(unused_imports)]
pub use deep_equals::{parse_query_string, CompiledDeepEquals, DeepEquals};
#[allow(unused_imports)]
pub use field_matcher::{
    compile_header_matcher, compile_query_matcher, CompiledFieldMatcher, CompiledFieldMatcherInner,
    CompiledHeaderMatcher, CompiledQueryMatcher, FieldMatcher, HeaderMatcher, QueryMatcher,
};
#[allow(unused_imports)]
pub use logical::{CompiledLogicalMatcher, LogicalMatcher};
#[allow(unused_imports)]
pub use matcher::{CachedValue, StringMatchCore};
#[allow(unused_imports)]
pub use options::PredicateOptions;
#[allow(unused_imports)]
pub use path_matcher::{CompiledPathMatch, CompiledPathMatcher, PathMatcher};
#[allow(unused_imports)]
pub use request::{CompiledRequestPredicate, RequestPredicate};
#[allow(unused_imports)]
pub use string_matcher::{CompiledExcept, CompiledStringMatcher, StringMatcher};

#[cfg(test)]
mod tests {
    // Tests are in individual modules
}
