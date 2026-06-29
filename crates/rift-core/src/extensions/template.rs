//! Request data extraction and response body templating.
//!
//! This module provides Mountebank-compatible template substitution for response bodies.
//! It supports extracting values from the request and inserting them into responses.
//!
//! # Supported Template Variables
//!
//! - `${request.path}` - The request path
//! - `${request.method}` - The HTTP method
//! - `${request.query.<name>}` - Query parameter value
//! - `${request.headers.<name>}` - Header value (case-insensitive)
//! - `${request.pathParams.<name>}` - Path parameter from route pattern
//! - `${request.body}` - The raw request body
//!
//! # Example
//!
//! ```yaml
//! fault:
//!   error:
//!     status: 200
//!     body: '{"echo": "${request.query.message}", "path": "${request.path}"}'
//! ```

use crate::predicate::parse_query_string;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Regex for matching template variables: ${request.path}, ${request.query.name}, etc.
static TEMPLATE_REGEX: OnceLock<Regex> = OnceLock::new();

fn get_template_regex() -> &'static Regex {
    TEMPLATE_REGEX.get_or_init(|| {
        Regex::new(r"\$\{request\.([a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_-]*)?)\}")
            .unwrap()
    })
}

/// Parsed request data for template substitution
#[derive(Debug, Clone, Default)]
pub struct RequestData {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Request path (without query string)
    pub path: String,
    /// Query parameters parsed from the URL
    pub query: HashMap<String, String>,
    /// Request headers (keys lowercased)
    pub headers: HashMap<String, String>,
    /// Path parameters extracted from route patterns (e.g., /users/:id)
    pub path_params: HashMap<String, String>,
    /// Raw request body
    pub body: String,
}

impl RequestData {
    /// Create RequestData from request components
    pub fn new(
        method: &str,
        path: &str,
        query_string: Option<&str>,
        headers: &hyper::HeaderMap,
        body: Option<&str>,
    ) -> Self {
        let query = parse_query_string(query_string);
        let headers_map = headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|val| (k.as_str().to_lowercase(), val.to_string()))
            })
            .collect();

        Self {
            method: method.to_string(),
            path: path.to_string(),
            query,
            headers: headers_map,
            path_params: HashMap::new(),
            body: body.unwrap_or("").to_string(),
        }
    }

    /// Get a value by dotted path (e.g., "query.name", "headers.content-type")
    pub fn get(&self, path: &str) -> Option<String> {
        let parts: Vec<&str> = path.splitn(2, '.').collect();

        match parts.as_slice() {
            ["path"] => Some(self.path.clone()),
            ["method"] => Some(self.method.clone()),
            ["body"] => Some(self.body.clone()),
            ["query", name] => self.query.get(*name).cloned(),
            ["headers", name] => self.headers.get(&name.to_lowercase()).cloned(),
            ["pathParams", name] | ["path_params", name] => self.path_params.get(*name).cloned(),
            _ => None,
        }
    }
}

/// Extract path parameters from a route pattern and actual path (used in tests)
#[cfg(test)]
fn extract_path_params(pattern: &str, path: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();

    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if pattern_parts.len() != path_parts.len() {
        return params;
    }

    for (pattern_part, path_part) in pattern_parts.iter().zip(path_parts.iter()) {
        if let Some(param_name) = pattern_part.strip_prefix(':') {
            params.insert(param_name.to_string(), path_part.to_string());
        } else if pattern_part != path_part {
            // Pattern doesn't match
            return HashMap::new();
        }
    }

    params
}

/// Process a template string, substituting variables with request data
///
/// # Arguments
/// * `template` - The template string containing `${request.*}` variables
/// * `request_data` - The request data to use for substitution
///
/// # Returns
/// The processed string with all variables substituted
pub fn process_template(template: &str, request_data: &RequestData) -> String {
    get_template_regex()
        .replace_all(template, |caps: &regex::Captures| {
            let var_path = &caps[1];
            request_data.get(var_path).unwrap_or_default()
        })
        .to_string()
}

/// Check if a string contains template variables
pub fn has_template_variables(s: &str) -> bool {
    get_template_regex().is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderName, HeaderValue};
    use hyper::HeaderMap;

    fn create_test_request_data() -> RequestData {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("req-12345"),
        );

        let mut data = RequestData::new(
            "POST",
            "/users/123",
            Some("name=John&age=30"),
            &headers,
            Some(r#"{"action": "test"}"#),
        );

        data.path_params.insert("id".to_string(), "123".to_string());
        data
    }

    #[test]
    fn test_parse_query_string() {
        let params = parse_query_string(Some("name=John&age=30&city=New%20York"));
        assert_eq!(params.get("name"), Some(&"John".to_string()));
        assert_eq!(params.get("age"), Some(&"30".to_string()));
        assert_eq!(params.get("city"), Some(&"New York".to_string()));
    }

    #[test]
    fn test_parse_query_string_empty() {
        let params = parse_query_string(None);
        assert!(params.is_empty());
    }

    #[test]
    fn test_extract_path_params() {
        let params = extract_path_params("/users/:id/posts/:post_id", "/users/123/posts/456");
        assert_eq!(params.get("id"), Some(&"123".to_string()));
        assert_eq!(params.get("post_id"), Some(&"456".to_string()));
    }

    #[test]
    fn test_extract_path_params_no_match() {
        let params = extract_path_params("/users/:id", "/posts/123");
        assert!(params.is_empty());
    }

    #[test]
    fn test_request_data_get() {
        let data = create_test_request_data();

        assert_eq!(data.get("path"), Some("/users/123".to_string()));
        assert_eq!(data.get("method"), Some("POST".to_string()));
        assert_eq!(data.get("query.name"), Some("John".to_string()));
        assert_eq!(data.get("query.age"), Some("30".to_string()));
        assert_eq!(
            data.get("headers.content-type"),
            Some("application/json".to_string())
        );
        assert_eq!(
            data.get("headers.x-request-id"),
            Some("req-12345".to_string())
        );
        assert_eq!(data.get("pathParams.id"), Some("123".to_string()));
        assert_eq!(data.get("body"), Some(r#"{"action": "test"}"#.to_string()));
    }

    #[test]
    fn test_process_template_path() {
        let data = create_test_request_data();
        let template = r#"{"path": "${request.path}"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"path": "/users/123"}"#);
    }

    #[test]
    fn test_process_template_query() {
        let data = create_test_request_data();
        let template = r#"{"name": "${request.query.name}", "age": "${request.query.age}"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"name": "John", "age": "30"}"#);
    }

    #[test]
    fn test_process_template_headers() {
        let data = create_test_request_data();
        let template = r#"{"requestId": "${request.headers.x-request-id}"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"requestId": "req-12345"}"#);
    }

    #[test]
    fn test_process_template_path_params() {
        let data = create_test_request_data();
        let template = r#"{"userId": "${request.pathParams.id}"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"userId": "123"}"#);
    }

    #[test]
    fn test_process_template_multiple() {
        let data = create_test_request_data();
        let template = r#"{"method": "${request.method}", "path": "${request.path}", "name": "${request.query.name}"}"#;
        let result = process_template(template, &data);
        assert_eq!(
            result,
            r#"{"method": "POST", "path": "/users/123", "name": "John"}"#
        );
    }

    #[test]
    fn test_process_template_missing_variable() {
        let data = create_test_request_data();
        let template = r#"{"missing": "${request.query.nonexistent}"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"missing": ""}"#);
    }

    #[test]
    fn test_process_template_no_variables() {
        let data = create_test_request_data();
        let template = r#"{"static": "value"}"#;
        let result = process_template(template, &data);
        assert_eq!(result, r#"{"static": "value"}"#);
    }

    #[test]
    fn test_has_template_variables() {
        assert!(has_template_variables("${request.path}"));
        assert!(has_template_variables(
            "prefix ${request.query.name} suffix"
        ));
        assert!(!has_template_variables("no variables here"));
        assert!(!has_template_variables("${invalid}"));
    }
}
