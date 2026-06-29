use crate::config::{HeaderMatch, PathMatch, Rule};
use crate::predicate::{
    compile_header_matcher, compile_query_matcher, parse_query_string, CompiledBodyMatcher,
    CompiledFieldMatcher,
};
use hyper::{HeaderMap, Method, Uri};
use regex::Regex;
use std::sync::Arc;

pub struct CompiledRule {
    pub id: String,
    pub match_config: CompiledMatch,
    pub rule: Arc<Rule>,
}

pub struct CompiledMatch {
    methods: Vec<Method>,
    path_matcher: PathMatcher,
    headers: Vec<HeaderMatch>,
    /// Enhanced header predicates (Mountebank-compatible)
    header_predicates: Vec<CompiledFieldMatcher>,
    /// Query parameter matchers
    query_matchers: Vec<CompiledFieldMatcher>,
    /// Body matcher
    body_matcher: Option<CompiledBodyMatcher>,
    /// Case-sensitive matching
    case_sensitive: bool,
}

enum PathMatcher {
    Any,
    Exact(String),
    Prefix(String),
    Regex(Regex),
    Contains(String),
    EndsWith(String),
}

impl CompiledRule {
    pub fn compile(rule: Rule) -> Result<Self, anyhow::Error> {
        let methods: Vec<Method> = rule
            .match_config
            .methods
            .iter()
            .map(|m| m.parse())
            .collect::<Result<Vec<_>, _>>()?;

        let path_matcher = match &rule.match_config.path {
            PathMatch::Any => PathMatcher::Any,
            PathMatch::Exact { exact } => PathMatcher::Exact(exact.clone()),
            PathMatch::Prefix { prefix } => PathMatcher::Prefix(prefix.clone()),
            PathMatch::Regex { regex } => PathMatcher::Regex(Regex::new(regex)?),
            PathMatch::Contains { contains } => PathMatcher::Contains(contains.clone()),
            PathMatch::EndsWith { ends_with } => PathMatcher::EndsWith(ends_with.clone()),
        };

        // Compile enhanced header predicates
        let header_predicates: Result<Vec<_>, _> = rule
            .match_config
            .header_predicates
            .iter()
            .map(compile_header_matcher)
            .collect();

        // Compile query matchers
        let query_matchers: Result<Vec<_>, _> = rule
            .match_config
            .query
            .iter()
            .map(compile_query_matcher)
            .collect();

        // Compile body matcher
        let body_matcher = rule
            .match_config
            .body
            .as_ref()
            .map(CompiledBodyMatcher::compile)
            .transpose()?;

        Ok(CompiledRule {
            id: rule.id.clone(),
            match_config: CompiledMatch {
                methods,
                path_matcher,
                headers: rule.match_config.headers.clone(),
                header_predicates: header_predicates?,
                query_matchers: query_matchers?,
                body_matcher,
                case_sensitive: rule.match_config.case_sensitive,
            },
            rule: Arc::new(rule),
        })
    }

    pub fn matches(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        self.matches_with_body(method, uri, headers, None)
    }

    /// Match with optional request body (for body predicates)
    pub fn matches_with_body(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: Option<&str>,
    ) -> bool {
        let case_sensitive = self.match_config.case_sensitive;

        // Match method
        if !self.match_config.methods.is_empty() && !self.match_config.methods.contains(method) {
            return false;
        }

        // Match path
        let path = uri.path();
        match &self.match_config.path_matcher {
            PathMatcher::Any => {}
            PathMatcher::Exact(exact) => {
                let matches = if case_sensitive {
                    path == exact
                } else {
                    path.eq_ignore_ascii_case(exact)
                };
                if !matches {
                    return false;
                }
            }
            PathMatcher::Prefix(prefix) => {
                let matches = if case_sensitive {
                    path.starts_with(prefix)
                } else {
                    path.to_lowercase().starts_with(&prefix.to_lowercase())
                };
                if !matches {
                    return false;
                }
            }
            PathMatcher::Regex(regex) => {
                if !regex.is_match(path) {
                    return false;
                }
            }
            PathMatcher::Contains(pattern) => {
                let matches = if case_sensitive {
                    path.contains(pattern)
                } else {
                    path.to_lowercase().contains(&pattern.to_lowercase())
                };
                if !matches {
                    return false;
                }
            }
            PathMatcher::EndsWith(suffix) => {
                let matches = if case_sensitive {
                    path.ends_with(suffix)
                } else {
                    path.to_lowercase().ends_with(&suffix.to_lowercase())
                };
                if !matches {
                    return false;
                }
            }
        }

        // Match simple headers (backward compatible)
        for header_match in &self.match_config.headers {
            let header_value = match headers.get(&header_match.name) {
                Some(value) => value,
                None => return false,
            };

            let value_str = match header_value.to_str() {
                Ok(s) => s,
                Err(_) => return false,
            };

            if value_str != header_match.value {
                return false;
            }
        }

        // Match enhanced header predicates
        for header_pred in &self.match_config.header_predicates {
            let value = headers
                .get(header_pred.name.as_str())
                .and_then(|v| v.to_str().ok());
            if !header_pred.matches(value) {
                return false;
            }
        }

        // Match query parameters
        if !self.match_config.query_matchers.is_empty() {
            let query_params = parse_query_string(uri.query());
            for query_matcher in &self.match_config.query_matchers {
                let value = query_params.get(&query_matcher.name).map(|s| s.as_str());
                if !query_matcher.matches(value) {
                    return false;
                }
            }
        }

        // Match body (if provided and body matcher configured)
        if let Some(ref body_matcher) = self.match_config.body_matcher {
            match body {
                Some(body_str) => {
                    if !body_matcher.matches(body_str, case_sensitive) {
                        return false;
                    }
                }
                None => {
                    // Body matcher configured but no body provided - don't match
                    return false;
                }
            }
        }

        true
    }
}

pub fn find_matching_rule<'a>(
    rules: &'a [CompiledRule],
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<&'a CompiledRule> {
    rules.iter().find(|rule| rule.matches(method, uri, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FaultConfig, LatencyFault, MatchConfig};

    fn create_test_rule(id: &str, methods: Vec<&str>, path: PathMatch) -> Rule {
        Rule {
            id: id.to_string(),
            match_config: MatchConfig {
                methods: methods.iter().map(|m| m.to_string()).collect(),
                path,
                headers: vec![],
                header_predicates: vec![],
                query: vec![],
                body: None,
                case_sensitive: true,
            },
            fault: FaultConfig {
                latency: Some(LatencyFault {
                    probability: 0.5,
                    min_ms: 100,
                    max_ms: 200,
                }),
                error: None,
                tcp_fault: None,
            },
            upstream: None, // No upstream filter for tests
        }
    }

    #[test]
    fn test_method_matching() {
        let rule = create_test_rule("test", vec!["POST"], PathMatch::Any);
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::POST, &uri, &headers));
        assert!(!compiled.matches(&Method::GET, &uri, &headers));
    }

    #[test]
    fn test_path_exact_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Exact {
                exact: "/api/v1/test".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        let uri1 = "http://localhost/api/v1/test".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri1, &headers));

        let uri2 = "http://localhost/api/v1/test2".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri2, &headers));

        let uri3 = "http://localhost/api/v1".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_path_prefix_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Prefix {
                prefix: "/api".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        let uri1 = "http://localhost/api/v1/test".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri1, &headers));

        let uri2 = "http://localhost/api".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri2, &headers));

        let uri3 = "http://localhost/other".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_path_regex_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Regex {
                regex: r"^/api/v\d+/.*".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        let uri1 = "http://localhost/api/v1/test".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri1, &headers));

        let uri2 = "http://localhost/api/v99/something".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri2, &headers));

        let uri3 = "http://localhost/api/test".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_combined_matching() {
        let rule = create_test_rule(
            "test",
            vec!["POST", "PUT"],
            PathMatch::Prefix {
                prefix: "/api".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/api/v1/test".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::POST, &uri, &headers));
        assert!(compiled.matches(&Method::PUT, &uri, &headers));
        assert!(!compiled.matches(&Method::GET, &uri, &headers));

        let uri2 = "http://localhost/other".parse().unwrap();
        assert!(!compiled.matches(&Method::POST, &uri2, &headers));
    }

    // ============================================
    // Additional tests for expanded coverage
    // ============================================

    #[test]
    fn test_empty_methods_matches_all() {
        let rule = create_test_rule("test", vec![], PathMatch::Any);
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/test".parse().unwrap();
        let headers = HeaderMap::new();

        // Empty methods list should match all HTTP methods
        assert!(compiled.matches(&Method::GET, &uri, &headers));
        assert!(compiled.matches(&Method::POST, &uri, &headers));
        assert!(compiled.matches(&Method::PUT, &uri, &headers));
        assert!(compiled.matches(&Method::DELETE, &uri, &headers));
        assert!(compiled.matches(&Method::PATCH, &uri, &headers));
        assert!(compiled.matches(&Method::HEAD, &uri, &headers));
        assert!(compiled.matches(&Method::OPTIONS, &uri, &headers));
    }

    #[test]
    fn test_case_insensitive_path_matching() {
        let mut rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Exact {
                exact: "/API/Users".to_string(),
            },
        );
        rule.match_config.case_sensitive = false;
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Case insensitive matching
        let uri1 = "http://localhost/api/users".parse().unwrap();
        let uri2 = "http://localhost/API/USERS".parse().unwrap();
        let uri3 = "http://localhost/Api/Users".parse().unwrap();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_case_sensitive_path_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Exact {
                exact: "/API/Users".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Case sensitive matching (default)
        let uri1 = "http://localhost/API/Users".parse().unwrap();
        let uri2 = "http://localhost/api/users".parse().unwrap();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(!compiled.matches(&Method::GET, &uri2, &headers));
    }

    #[test]
    fn test_header_matching_simple() {
        use crate::config::HeaderMatch;

        let mut rule = create_test_rule("test", vec!["GET"], PathMatch::Any);
        rule.match_config.headers = vec![HeaderMatch {
            name: "X-Custom-Header".to_string(),
            value: "expected-value".to_string(),
        }];
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/test".parse().unwrap();

        // With matching header
        let mut headers = HeaderMap::new();
        headers.insert("X-Custom-Header", "expected-value".parse().unwrap());
        assert!(compiled.matches(&Method::GET, &uri, &headers));

        // With wrong header value
        let mut headers2 = HeaderMap::new();
        headers2.insert("X-Custom-Header", "wrong-value".parse().unwrap());
        assert!(!compiled.matches(&Method::GET, &uri, &headers2));

        // Without required header
        let headers3 = HeaderMap::new();
        assert!(!compiled.matches(&Method::GET, &uri, &headers3));
    }

    #[test]
    fn test_multiple_header_matching() {
        use crate::config::HeaderMatch;

        let mut rule = create_test_rule("test", vec![], PathMatch::Any);
        rule.match_config.headers = vec![
            HeaderMatch {
                name: "X-Header-1".to_string(),
                value: "value1".to_string(),
            },
            HeaderMatch {
                name: "X-Header-2".to_string(),
                value: "value2".to_string(),
            },
        ];
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/test".parse().unwrap();

        // Both headers present and correct
        let mut headers = HeaderMap::new();
        headers.insert("X-Header-1", "value1".parse().unwrap());
        headers.insert("X-Header-2", "value2".parse().unwrap());
        assert!(compiled.matches(&Method::GET, &uri, &headers));

        // Only first header present
        let mut headers2 = HeaderMap::new();
        headers2.insert("X-Header-1", "value1".parse().unwrap());
        assert!(!compiled.matches(&Method::GET, &uri, &headers2));

        // Both present but one has wrong value
        let mut headers3 = HeaderMap::new();
        headers3.insert("X-Header-1", "value1".parse().unwrap());
        headers3.insert("X-Header-2", "wrong".parse().unwrap());
        assert!(!compiled.matches(&Method::GET, &uri, &headers3));
    }

    #[test]
    fn test_path_contains_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Contains {
                contains: "admin".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Contains "admin"
        let uri1 = "http://localhost/api/admin/users".parse().unwrap();
        let uri2 = "http://localhost/admin".parse().unwrap();
        let uri3 = "http://localhost/path/to/admin/page".parse().unwrap();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(compiled.matches(&Method::GET, &uri3, &headers));

        // Does not contain "admin"
        let uri4 = "http://localhost/api/users".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri4, &headers));
    }

    #[test]
    fn test_path_ends_with_matching() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::EndsWith {
                ends_with: ".json".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Ends with .json
        let uri1 = "http://localhost/api/data.json".parse().unwrap();
        let uri2 = "http://localhost/config.json".parse().unwrap();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));

        // Does not end with .json
        let uri3 = "http://localhost/api/data.xml".parse().unwrap();
        let uri4 = "http://localhost/api/json/data".parse().unwrap();

        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
        assert!(!compiled.matches(&Method::GET, &uri4, &headers));
    }

    #[test]
    fn test_compiled_rule_id_preserved() {
        let rule = create_test_rule("my-unique-rule-id", vec!["GET"], PathMatch::Any);
        let compiled = CompiledRule::compile(rule).unwrap();
        assert_eq!(compiled.id, "my-unique-rule-id");
    }

    #[test]
    fn test_compiled_rule_with_body_matcher() {
        use crate::predicate::BodyMatcher;

        let mut rule = create_test_rule("test", vec![], PathMatch::Any);
        rule.match_config.body = Some(BodyMatcher::Contains("important".to_string()));
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri = "http://localhost/test".parse().unwrap();
        let headers = HeaderMap::new();

        // Without body - should not match when body matcher is configured
        assert!(!compiled.matches(&Method::POST, &uri, &headers));

        // With body containing "important"
        assert!(compiled.matches_with_body(
            &Method::POST,
            &uri,
            &headers,
            Some("This is important data")
        ));

        // With body not containing "important"
        assert!(!compiled.matches_with_body(
            &Method::POST,
            &uri,
            &headers,
            Some("This is regular data")
        ));
    }

    #[test]
    fn test_invalid_regex_compilation() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Regex {
                regex: "[invalid(regex".to_string(), // Invalid regex
            },
        );
        let result = CompiledRule::compile(rule);
        assert!(result.is_err());
    }

    #[test]
    fn test_custom_method_compilation() {
        // Note: hyper allows any string as HTTP method, so custom methods work
        let rule = create_test_rule("test", vec!["CUSTOM_METHOD"], PathMatch::Any);
        let result = CompiledRule::compile(rule);
        // This actually succeeds because hyper allows custom methods
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_matching_rule_helper() {
        let rules = vec![
            create_test_rule(
                "rule-1",
                vec!["GET"],
                PathMatch::Prefix {
                    prefix: "/api".to_string(),
                },
            ),
            create_test_rule(
                "rule-2",
                vec!["POST"],
                PathMatch::Exact {
                    exact: "/submit".to_string(),
                },
            ),
        ];

        let compiled_rules: Vec<CompiledRule> = rules
            .into_iter()
            .map(|r| CompiledRule::compile(r).unwrap())
            .collect();

        let headers = HeaderMap::new();

        // Should match rule-1
        let uri1 = "http://localhost/api/users".parse().unwrap();
        let matched1 = find_matching_rule(&compiled_rules, &Method::GET, &uri1, &headers);
        assert!(matched1.is_some());
        assert_eq!(matched1.unwrap().id, "rule-1");

        // Should match rule-2
        let uri2 = "http://localhost/submit".parse().unwrap();
        let matched2 = find_matching_rule(&compiled_rules, &Method::POST, &uri2, &headers);
        assert!(matched2.is_some());
        assert_eq!(matched2.unwrap().id, "rule-2");

        // Should not match any rule
        let uri3 = "http://localhost/other".parse().unwrap();
        let matched3 = find_matching_rule(&compiled_rules, &Method::GET, &uri3, &headers);
        assert!(matched3.is_none());
    }

    #[test]
    fn test_complex_regex_pattern() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Regex {
                regex: r"^/api/v(\d+)/users/([a-zA-Z0-9_-]+)$".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Matching patterns
        let uri1 = "http://localhost/api/v1/users/john123".parse().unwrap();
        let uri2 = "http://localhost/api/v99/users/user_name-123"
            .parse()
            .unwrap();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));

        // Non-matching patterns
        let uri3 = "http://localhost/api/v1/users".parse().unwrap();
        let uri4 = "http://localhost/api/v/users/john".parse().unwrap();
        let uri5 = "http://localhost/api/v1/users/john/extra".parse().unwrap();

        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
        assert!(!compiled.matches(&Method::GET, &uri4, &headers));
        assert!(!compiled.matches(&Method::GET, &uri5, &headers));
    }

    #[test]
    fn test_rule_with_upstream_filter() {
        let mut rule = create_test_rule("test", vec!["GET"], PathMatch::Any);
        rule.upstream = Some("backend-a".to_string());
        let compiled = CompiledRule::compile(rule).unwrap();

        assert_eq!(compiled.rule.upstream, Some("backend-a".to_string()));
    }

    #[test]
    fn test_prefix_matching_edge_cases() {
        let rule = create_test_rule(
            "test",
            vec![],
            PathMatch::Prefix {
                prefix: "/api".to_string(),
            },
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let headers = HeaderMap::new();

        // Exact match of prefix
        let uri1 = "http://localhost/api".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri1, &headers));

        // Prefix with trailing slash
        let uri2 = "http://localhost/api/".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri2, &headers));

        // Prefix with path
        let uri3 = "http://localhost/api/v1/users".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri3, &headers));

        // Similar but not prefix
        let uri4 = "http://localhost/apiV2".parse().unwrap();
        assert!(compiled.matches(&Method::GET, &uri4, &headers)); // Still matches since "apiV2" starts with "api"

        // Different path
        let uri5 = "http://localhost/other/api".parse().unwrap();
        assert!(!compiled.matches(&Method::GET, &uri5, &headers));
    }
}
