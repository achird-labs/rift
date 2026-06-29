//! Unified request predicate for matching against request fields.

use super::body_matcher::{BodyMatcher, CompiledBodyMatcher};
use super::field_matcher::{compile_header_matcher, compile_query_matcher, FieldMatcher};
use super::options::PredicateOptions;
use super::path_matcher::{CompiledPathMatch, PathMatcher};
use super::string_matcher::{CompiledStringMatcher, StringMatcher};
use serde::{Deserialize, Serialize};

/// A complete request predicate that can match against various request fields.
///
/// This is the main predicate type used in rule matching, combining all
/// supported matching capabilities.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RequestPredicate {
    /// HTTP method match
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<StringMatcher>,

    /// Path match
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathMatcher>,

    /// Header matchers (all must match)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<FieldMatcher>,

    /// Query parameter matchers (all must match)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<FieldMatcher>,

    /// Body matcher
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyMatcher>,

    /// Global predicate options
    #[serde(flatten, default)]
    pub options: PredicateOptions,
}

/// Compiled request predicate for efficient runtime evaluation.
#[derive(Debug, Clone)]
pub struct CompiledRequestPredicate {
    pub method: Option<CompiledStringMatcher>,
    pub path: Option<CompiledPathMatch>,
    pub headers: Vec<super::field_matcher::CompiledFieldMatcher>,
    pub query: Vec<super::field_matcher::CompiledFieldMatcher>,
    pub body: Option<CompiledBodyMatcher>,
    pub case_sensitive: bool,
}

impl CompiledRequestPredicate {
    /// Compile a RequestPredicate configuration.
    pub fn compile(predicate: &RequestPredicate) -> Result<Self, regex::Error> {
        let method = predicate
            .method
            .as_ref()
            .map(CompiledStringMatcher::compile)
            .transpose()?;

        let path = predicate
            .path
            .as_ref()
            .map(CompiledPathMatch::compile)
            .transpose()?;

        let headers: Result<Vec<_>, _> = predicate
            .headers
            .iter()
            .map(compile_header_matcher)
            .collect();

        let query: Result<Vec<_>, _> = predicate.query.iter().map(compile_query_matcher).collect();

        let body = predicate
            .body
            .as_ref()
            .map(CompiledBodyMatcher::compile)
            .transpose()?;

        Ok(CompiledRequestPredicate {
            method,
            path,
            headers: headers?,
            query: query?,
            body,
            case_sensitive: predicate.options.case_sensitive,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_predicate_compile() {
        let predicate = RequestPredicate {
            method: Some(StringMatcher::Equals("GET".to_string())),
            path: Some(PathMatcher::Prefix {
                prefix: "/api".to_string(),
            }),
            headers: vec![FieldMatcher::Simple {
                name: "Content-Type".to_string(),
                value: "application/json".to_string(),
            }],
            query: vec![FieldMatcher::Simple {
                name: "page".to_string(),
                value: "1".to_string(),
            }],
            body: None,
            options: PredicateOptions::default(),
        };

        let compiled = CompiledRequestPredicate::compile(&predicate);
        assert!(compiled.is_ok());

        let compiled = compiled.unwrap();
        assert!(compiled.method.is_some());
        assert!(compiled.path.is_some());
        assert_eq!(compiled.headers.len(), 1);
        assert_eq!(compiled.query.len(), 1);
    }
}
