//! Copy behavior - copy request fields into response.

use super::extraction::ExtractionMethod;
use super::request::RequestContext;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Copy behavior - copy request fields into response
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyBehavior {
    /// Request field to copy from
    pub from: CopySource,
    /// Response token to replace (e.g., "${NAME}")
    pub into: String,
    /// Extraction method
    #[serde(rename = "using")]
    pub extraction: ExtractionMethod,
}

/// Source of data to copy from request
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CopySource {
    /// Simple field: "path", "body", "method"
    Simple(String),
    /// Nested field: {"query": "name"} or {"headers": "Content-Type"}
    Nested(HashMap<String, String>),
}

impl CopySource {
    /// Extract value from request data
    pub fn extract(&self, request: &RequestContext) -> Option<String> {
        match self {
            CopySource::Simple(field) => match field.as_str() {
                "path" => Some(request.path.clone()),
                "method" => Some(request.method.clone()),
                "body" => request.body.clone(),
                _ => None,
            },
            CopySource::Nested(map) => {
                if let Some(param_name) = map.get("query") {
                    request.query.get(param_name).cloned()
                } else if let Some(header_name) = map.get("headers") {
                    // Case-insensitive header lookup since HTTP headers are case-insensitive
                    let lower_name = header_name.to_lowercase();
                    request
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == lower_name)
                        .map(|(_, v)| v.clone())
                } else {
                    None
                }
            }
        }
    }
}

/// Apply copy behaviors to response body
pub fn apply_copy_behaviors(
    body: &str,
    headers: &mut HashMap<String, Vec<String>>,
    behaviors: &[CopyBehavior],
    request: &RequestContext,
) -> String {
    let mut result = body.to_string();

    for behavior in behaviors {
        // Extract value from request
        if let Some(source_value) = behavior.from.extract(request) {
            // Apply extraction method
            let extracted = behavior.extraction.extract(&source_value);
            let replacement = extracted.unwrap_or_default();

            // Replace token in body
            result = result.replace(&behavior.into, &replacement);

            // Also replace in headers — per value, so multi-value headers (e.g. multiple
            // Set-Cookie) keep their multiplicity (RFC 7230 §3.2.2 forbids folding Set-Cookie).
            for value in headers.values_mut().flatten() {
                *value = value.replace(&behavior.into, &replacement);
            }
        } else {
            // Source not found, replace with empty string
            result = result.replace(&behavior.into, "");
            for value in headers.values_mut().flatten() {
                *value = value.replace(&behavior.into, "");
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_source_simple() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some("test body".to_string()),
        };

        let source = CopySource::Simple("path".to_string());
        assert_eq!(source.extract(&request), Some("/users/123".to_string()));

        let source = CopySource::Simple("method".to_string());
        assert_eq!(source.extract(&request), Some("GET".to_string()));

        let source = CopySource::Simple("body".to_string());
        assert_eq!(source.extract(&request), Some("test body".to_string()));
    }

    #[test]
    fn test_copy_source_nested() {
        let mut query = HashMap::new();
        query.insert("name".to_string(), "Alice".to_string());

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query,
            headers,
            body: None,
        };

        let mut map = HashMap::new();
        map.insert("query".to_string(), "name".to_string());
        let source = CopySource::Nested(map);
        assert_eq!(source.extract(&request), Some("Alice".to_string()));

        let mut map = HashMap::new();
        map.insert("headers".to_string(), "Content-Type".to_string());
        let source = CopySource::Nested(map);
        assert_eq!(
            source.extract(&request),
            Some("application/json".to_string())
        );
    }

    #[test]
    fn test_apply_copy_behaviors() {
        let mut query = HashMap::new();
        query.insert("name".to_string(), "Alice".to_string());

        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
        };

        let behaviors = vec![
            CopyBehavior {
                from: CopySource::Simple("path".to_string()),
                into: "${PATH}".to_string(),
                extraction: ExtractionMethod::Regex {
                    selector: r"/users/(\d+)".to_string(),
                    options: None,
                },
            },
            CopyBehavior {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "name".to_string());
                    CopySource::Nested(map)
                },
                into: "${NAME}".to_string(),
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
            },
        ];

        let body = r#"{"userId": "${PATH}", "greeting": "Hello, ${NAME}!"}"#;
        let mut headers = HashMap::new();

        let result = apply_copy_behaviors(body, &mut headers, &behaviors, &request);
        assert_eq!(result, r#"{"userId": "123", "greeting": "Hello, Alice!"}"#);
    }

    #[test]
    fn copy_preserves_multi_value_headers_and_substitutes_each() {
        let mut query = HashMap::new();
        query.insert("q".to_string(), "hi".to_string());
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: HashMap::new(),
            body: None,
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "b=${q}".to_string()],
        );

        apply_copy_behaviors("", &mut headers, &behaviors, &request);

        // Both cookie lines survive (no fold) and the token is substituted in place.
        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "b=hi".to_string()]
        );
    }

    #[test]
    fn copy_missing_source_clears_token_per_value_without_folding() {
        // No `q` query param → the source is absent, so the token is replaced with "" in each
        // value; the untouched value and the multiplicity both survive.
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        let behaviors = vec![CopyBehavior {
            from: {
                let mut map = HashMap::new();
                map.insert("query".to_string(), "q".to_string());
                CopySource::Nested(map)
            },
            into: "${q}".to_string(),
            extraction: ExtractionMethod::Regex {
                selector: ".*".to_string(),
                options: None,
            },
        }];

        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "b=${q}".to_string()],
        );

        apply_copy_behaviors("", &mut headers, &behaviors, &request);

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "b=".to_string()]
        );
    }
}
