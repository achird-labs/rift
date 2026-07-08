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

    /// Populate `path_params` by matching the request path against a route `pattern` (issue #433),
    /// e.g. `/users/:id`. A `None` pattern — or one whose shape doesn't match the path — leaves the
    /// map empty (the unchanged default), so callers can pass a stub's optional pattern directly.
    #[must_use]
    pub fn with_route_pattern(mut self, pattern: Option<&str>) -> Self {
        if let Some(pattern) = pattern {
            self.path_params = extract_path_params(pattern, &self.path);
        }
        self
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

/// Extract path parameters from a route `pattern` (`:name` segments) and the actual request
/// `path`. Returns an empty map when the segment counts differ or a literal segment doesn't match.
pub(crate) fn extract_path_params(pattern: &str, path: &str) -> HashMap<String, String> {
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

static DAYS_REGEX: OnceLock<Regex> = OnceLock::new();
static MONTHS_REGEX: OnceLock<Regex> = OnceLock::new();
static NOW_REGEX: OnceLock<Regex> = OnceLock::new();

/// Expand legacy-recorder relative-date templates in a response body (issue #195):
/// `{{DAYS+N}}` / `{{DAYS-N}}` / `{{MONTHS+N}}` / `{{MONTHS-N}}` → an RFC3339 timestamp `N`
/// days/months in the future (`+`) or past (`-`, issue #270) from now, and `{{NOW}}` → the current
/// UTC timestamp. A token whose offset overflows the representable date range is left unchanged
/// rather than panicking. These are a legacy extension, not standard Mountebank/WireMock.
pub fn apply_date_templates(body: &str) -> String {
    let now = chrono::Utc::now();
    let days_re = DAYS_REGEX.get_or_init(|| Regex::new(r"\{\{DAYS([+-])(\d+)\}\}").unwrap());
    let months_re = MONTHS_REGEX.get_or_init(|| Regex::new(r"\{\{MONTHS([+-])(\d+)\}\}").unwrap());
    let now_re = NOW_REGEX.get_or_init(|| Regex::new(r"\{\{NOW\}\}").unwrap());

    let with_days = days_re.replace_all(body, |caps: &regex::Captures| {
        match caps[2].parse::<i64>() {
            // `Duration::days` panics on overflow, so go through the fallible `try_days` and let an
            // out-of-range offset flow into the leave-token-unchanged fallback below.
            Ok(n) => {
                let signed = if &caps[1] == "-" { -n } else { n };
                chrono::Duration::try_days(signed)
                    .and_then(|d| now.checked_add_signed(d))
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| caps[0].to_string())
            }
            Err(_) => caps[0].to_string(),
        }
    });
    let with_months = months_re.replace_all(&with_days, |caps: &regex::Captures| {
        match caps[2].parse::<u32>() {
            Ok(n) => {
                let months = chrono::Months::new(n);
                let shifted = if &caps[1] == "-" {
                    now.checked_sub_months(months)
                } else {
                    now.checked_add_months(months)
                };
                shifted
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| caps[0].to_string())
            }
            Err(_) => caps[0].to_string(),
        }
    });
    now_re
        .replace_all(&with_months, |_: &regex::Captures| now.to_rfc3339())
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::HeaderMap;
    use hyper::header::{HeaderName, HeaderValue};

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
    fn test_with_route_pattern_builder() {
        let headers = hyper::HeaderMap::new();
        // A matching pattern populates path_params from the request path.
        let data = RequestData::new("GET", "/users/42", None, &headers, None)
            .with_route_pattern(Some("/users/:id"));
        assert_eq!(data.get("pathParams.id"), Some("42".to_string()));
        // No pattern (and a non-matching pattern) leave the map empty.
        let none =
            RequestData::new("GET", "/users/42", None, &headers, None).with_route_pattern(None);
        assert!(none.path_params.is_empty());
        let mismatch = RequestData::new("GET", "/orders/42", None, &headers, None)
            .with_route_pattern(Some("/users/:id"));
        assert!(mismatch.path_params.is_empty());
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

    // Issue #195: relative-date template expansion.
    use chrono::{DateTime, Months, Utc};

    fn parse_rfc3339(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("date template must produce valid RFC3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn date_tpl_days_zero_is_today() {
        let out = apply_date_templates("{{DAYS+0}}");
        assert_eq!(parse_rfc3339(&out).date_naive(), Utc::now().date_naive());
    }

    #[test]
    fn date_tpl_days_plus_offsets_by_days() {
        let out = apply_date_templates("{{DAYS+5}}");
        let expected = (Utc::now() + chrono::Duration::days(5)).date_naive();
        assert_eq!(parse_rfc3339(&out).date_naive(), expected);
    }

    #[test]
    fn date_tpl_months_plus_offsets_by_months() {
        let out = apply_date_templates("{{MONTHS+1}}");
        let expected = (Utc::now() + Months::new(1)).date_naive();
        assert_eq!(parse_rfc3339(&out).date_naive(), expected);
    }

    #[test]
    fn date_tpl_now_is_parseable() {
        let out = apply_date_templates("{{NOW}}");
        // Within a second of now.
        let delta = (Utc::now() - parse_rfc3339(&out)).num_seconds().abs();
        assert!(delta <= 2, "{{NOW}} should be ~now, delta={delta}s");
    }

    #[test]
    fn date_tpl_no_tokens_unchanged() {
        let body = r#"{"plain":"value","n":42}"#;
        assert_eq!(apply_date_templates(body), body);
    }

    #[test]
    fn date_tpl_mixed_resolves_dates_leaves_request_vars() {
        let body = r#"{"exp":"{{DAYS+5}}","who":"${request.path}"}"#;
        let out = apply_date_templates(body);
        assert!(!out.contains("{{DAYS+5}}"), "date token must be expanded");
        assert!(
            out.contains("${request.path}"),
            "only date tokens are expanded; other content (e.g. ${{...}}) is left untouched"
        );
    }

    #[test]
    fn date_tpl_multiple_tokens_in_one_body() {
        let out = apply_date_templates(r#"{"a":"{{DAYS+1}}","b":"{{MONTHS+2}}","c":"{{NOW}}"}"#);
        assert!(!out.contains("{{"), "all date tokens expanded: {out}");
    }

    #[test]
    fn date_tpl_days_overflow_leaves_token_unchanged() {
        // Parses as i64 but overflows the date range — must not panic (issue #195 no-panic contract).
        let body = "{{DAYS+9999999999999}}";
        assert_eq!(apply_date_templates(body), body);
    }

    #[test]
    fn date_tpl_months_overflow_leaves_token_unchanged() {
        let body = "{{MONTHS+4000000000}}";
        assert_eq!(apply_date_templates(body), body);
    }

    // Issue #270: past offsets.
    #[test]
    fn date_tpl_days_minus_offsets_into_past() {
        let out = apply_date_templates("{{DAYS-5}}");
        let expected = (Utc::now() - chrono::Duration::days(5)).date_naive();
        assert_eq!(parse_rfc3339(&out).date_naive(), expected);
    }

    #[test]
    fn date_tpl_months_minus_offsets_into_past() {
        let out = apply_date_templates("{{MONTHS-1}}");
        let expected = Utc::now()
            .checked_sub_months(Months::new(1))
            .expect("1 month ago is representable")
            .date_naive();
        assert_eq!(parse_rfc3339(&out).date_naive(), expected);
    }

    #[test]
    fn date_tpl_days_minus_zero_is_today() {
        let out = apply_date_templates("{{DAYS-0}}");
        assert_eq!(parse_rfc3339(&out).date_naive(), Utc::now().date_naive());
    }

    #[test]
    fn date_tpl_days_minus_overflow_leaves_token_unchanged() {
        let body = "{{DAYS-9999999999999}}";
        assert_eq!(apply_date_templates(body), body);
    }

    #[test]
    fn date_tpl_months_minus_underflow_leaves_token_unchanged() {
        // Subtractive `checked_sub_months` underflow must leave the token literal (no panic).
        let body = "{{MONTHS-4000000000}}";
        assert_eq!(apply_date_templates(body), body);
    }

    #[test]
    fn date_tpl_mixed_signs_resolve_per_occurrence() {
        let out = apply_date_templates("{{DAYS-5}}|{{DAYS+5}}");
        let parts: Vec<&str> = out.split('|').collect();
        assert_eq!(parts.len(), 2, "both tokens expand: {out}");
        assert_eq!(
            parse_rfc3339(parts[0]).date_naive(),
            (Utc::now() - chrono::Duration::days(5)).date_naive()
        );
        assert_eq!(
            parse_rfc3339(parts[1]).date_naive(),
            (Utc::now() + chrono::Duration::days(5)).date_naive()
        );
    }
}
