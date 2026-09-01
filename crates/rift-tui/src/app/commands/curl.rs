//! Curl command generation for App

use super::super::*;

impl App {
    /// Generate a curl command for a stub
    pub fn generate_curl_command(&self, stub: &Stub, port: u16) -> String {
        let mut parts = CurlRequestParts::default();

        // Parse predicates to extract request info
        for predicate in &stub.predicates {
            self.extract_from_predicate(predicate, &mut parts);
        }

        let CurlRequestParts {
            method,
            path,
            headers,
            query_params,
            json_body_parts,
            raw_body,
        } = parts;

        // Build final body - combine jsonpath parts into one JSON object
        let body = if !json_body_parts.is_empty() {
            Some(self.merge_jsonpath_bodies(&json_body_parts))
        } else {
            raw_body
        };

        // Build the curl command
        let mut parts: Vec<String> = vec!["curl -s".to_string()];

        // Add method if not GET
        if method != "GET" {
            parts.push(format!("-X {method}"));
        }

        // Add Content-Type header if we have a body and it looks like JSON
        if body.is_some() {
            let has_content_type = headers
                .iter()
                .any(|(k, _)| k.to_lowercase() == "content-type");
            if !has_content_type
                && let Some(ref b) = body
                && (b.trim_start().starts_with('{') || b.trim_start().starts_with('['))
            {
                parts.push("-H 'Content-Type: application/json'".to_string());
            }
        }

        // Add headers
        for (key, value) in &headers {
            parts.push(format!("-H '{key}: {value}'"));
        }

        // Add body if present
        if let Some(ref b) = body {
            parts.push(format!("-d '{}'", b.replace('\'', "'\\''")));
        }

        // Build URL with query params
        let mut url = format!("http://localhost:{port}{path}");
        if !query_params.is_empty() {
            let query_string: Vec<String> = query_params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            url = format!("{}?{}", url, query_string.join("&"));
        }

        parts.push(format!("'{url}'"));

        parts.join(" \\\n  ")
    }

    /// Extract request info from a predicate
    fn extract_from_predicate(&self, predicate: &serde_json::Value, parts: &mut CurlRequestParts) {
        if let Some(obj) = predicate.as_object() {
            // Check for jsonpath - if present, we need to build a JSON body
            let jsonpath_selector = obj
                .get("jsonpath")
                .and_then(|jp| jp.as_object())
                .and_then(|jp| jp.get("selector"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());

            // Handle different predicate types: equals, contains, startsWith, deepEquals, matches, etc.
            for (pred_type, pred_value) in obj {
                if pred_type == "and" || pred_type == "or" {
                    // Handle composite predicates
                    if let Some(arr) = pred_value.as_array() {
                        for sub_pred in arr {
                            self.extract_from_predicate(sub_pred, parts);
                        }
                    }
                    continue;
                }

                // Skip non-predicate fields
                if pred_type == "jsonpath" || pred_type == "caseSensitive" || pred_type == "except"
                {
                    continue;
                }

                if let Some(inner) = pred_value.as_object() {
                    // Extract method
                    if let Some(m) = inner.get("method").and_then(|v| v.as_str()) {
                        parts.method = m.to_uppercase();
                    }

                    // Extract path - handle equals, deepEquals, contains, matches
                    if let Some(p) = inner.get("path").and_then(|v| v.as_str()) {
                        let extracted_path = match pred_type.as_str() {
                            "matches" => {
                                // Convert regex pattern to a sample path
                                self.regex_to_sample_path(p)
                            }
                            "contains" | "startsWith" | "endsWith" => {
                                // Use the partial path, ensuring it starts with /
                                if p.starts_with('/') {
                                    p.to_string()
                                } else {
                                    format!("/{p}")
                                }
                            }
                            _ => {
                                // equals, deepEquals - use exact path
                                if p.starts_with('/') {
                                    p.to_string()
                                } else {
                                    format!("/{p}")
                                }
                            }
                        };
                        // Only update if we have a more specific path
                        if parts.path == "/" || extracted_path.len() > parts.path.len() {
                            parts.path = extracted_path;
                        }
                    }

                    // Extract headers
                    if let Some(hdrs) = inner.get("headers").and_then(|v| v.as_object()) {
                        for (k, v) in hdrs {
                            if let Some(val) = v.as_str() {
                                parts.headers.push((k.clone(), val.to_string()));
                            }
                        }
                    }

                    // Extract query parameters
                    if let Some(q) = inner.get("query").and_then(|v| v.as_object()) {
                        for (k, v) in q {
                            if let Some(val) = v.as_str() {
                                parts.query_params.push((k.clone(), val.to_string()));
                            }
                        }
                    }

                    // Extract body - handle jsonpath case
                    if let Some(b) = inner.get("body") {
                        if let Some(ref selector) = jsonpath_selector {
                            // Collect jsonpath body parts to merge later
                            parts.json_body_parts.push((selector.clone(), b.clone()));
                        } else if let Some(s) = b.as_str() {
                            // Plain string body
                            parts.raw_body = Some(s.to_string());
                        } else {
                            // Already a JSON object
                            parts.raw_body = Some(serde_json::to_string(b).unwrap_or_default());
                        }
                    }
                }
            }
        }
    }

    /// Convert a regex pattern to a sample path
    /// e.g., "^/auto/dealers/[^/]+/dealer-customers/[^/]+$" -> "/auto/dealers/{1}/dealer-customers/{2}"
    fn regex_to_sample_path(&self, pattern: &str) -> String {
        let mut path = pattern.to_string();

        // Remove regex anchors
        path = path
            .trim_start_matches('^')
            .trim_end_matches('$')
            .to_string();

        // Replace [^/]+ patterns with numbered placeholders
        let mut counter = 1;
        while path.contains("[^/]+") {
            path = path.replacen("[^/]+", &format!("{{{counter}}}"), 1);
            counter += 1;
        }

        // Replace other common regex patterns
        path = path.replace(r"\d+", "123");
        path = path.replace(".+", "sample");
        path = path.replace(".*", "");
        path = path.replace(r"\.", ".");
        path = path.replace(r"\/", "/");
        path = path.replace("(?:", "");
        path = path.replace(")?", "");
        path = path.replace("(", "");
        path = path.replace(")", "");

        if !path.starts_with('/') {
            path = format!("/{path}");
        }

        path
    }

    /// Merge multiple jsonpath body parts into a single JSON object
    /// e.g., [("$.user.id", "123"), ("$.user.name", "john")] -> {"user": {"id": "123", "name": "john"}}
    fn merge_jsonpath_bodies(&self, parts: &[(String, serde_json::Value)]) -> String {
        let mut root = serde_json::Map::new();

        for (selector, value) in parts {
            self.set_jsonpath_value(&mut root, selector, value.clone());
        }

        serde_json::to_string(&serde_json::Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
    }

    /// Set a value at a jsonpath location in a JSON object
    /// Handles array notation like [:0] by wrapping values in arrays
    /// e.g., $.receiver.context.correlationKeys.[:0].keyValue with "728839"
    ///       -> {"receiver":{"context":{"correlationKeys":[{"keyValue":"728839"}]}}}
    fn set_jsonpath_value(
        &self,
        root: &mut serde_json::Map<String, serde_json::Value>,
        selector: &str,
        value: serde_json::Value,
    ) {
        let path = selector.trim_start_matches('$').trim_start_matches('.');
        if path.is_empty() {
            return;
        }

        // Parse parts - each part can have array notation like "correlationKeys[:0]"
        // The part BEFORE the array index should become an array
        let raw_parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();

        // Build structure from leaf to root
        let mut current = value;

        for i in (0..raw_parts.len()).rev() {
            let part = raw_parts[i];

            // Check if this part has array notation (means we're inside an array)
            if part.starts_with("[:") || part.starts_with('[') {
                // This is an array index - wrap in array
                current = serde_json::json!([current]);
                continue;
            }

            // Check if part has embedded array notation like "correlationKeys[:0]"
            let (field_name, has_array) = if let Some(bracket_pos) = part.find('[') {
                (&part[..bracket_pos], true)
            } else {
                (part, false)
            };

            if field_name.is_empty() {
                continue;
            }

            // Wrap current value in an object with this field name
            let mut obj = serde_json::Map::new();
            if has_array {
                obj.insert(field_name.to_string(), serde_json::json!([current]));
            } else {
                obj.insert(field_name.to_string(), current);
            }
            current = serde_json::Value::Object(obj);
        }

        // Merge the built structure into root
        if let serde_json::Value::Object(built) = current {
            self.deep_merge(root, built);
        }
    }

    /// Deep merge two JSON objects
    fn deep_merge(
        &self,
        target: &mut serde_json::Map<String, serde_json::Value>,
        source: serde_json::Map<String, serde_json::Value>,
    ) {
        for (key, value) in source {
            match (target.get_mut(&key), value) {
                (Some(serde_json::Value::Object(t)), serde_json::Value::Object(s)) => {
                    self.deep_merge(t, s);
                }
                (Some(serde_json::Value::Array(t)), serde_json::Value::Array(s)) => {
                    // Merge array contents - for arrays of objects, merge first elements
                    if let Some(serde_json::Value::Object(t_obj)) = t.first_mut()
                        && let Some(serde_json::Value::Object(s_obj)) = s.into_iter().next()
                    {
                        self.deep_merge(t_obj, s_obj);
                    }
                }
                (_, v) => {
                    target.insert(key, v);
                }
            }
        }
    }

    /// Copy curl command for selected stub to clipboard
    pub fn copy_stub_as_curl(&mut self) {
        let port = match &self.view {
            View::ImposterDetail { port } => *port,
            View::StubDetail { port, .. } => *port,
            _ => return,
        };

        let stub_index = match &self.view {
            View::StubDetail { index, .. } => Some(*index),
            View::ImposterDetail { .. } => self.stub_list_state.selected(),
            _ => None,
        };

        if let Some(idx) = stub_index
            && let Some(imp) = &self.current_imposter
            && let Some(stub) = imp.stubs.get(idx)
        {
            let curl_cmd = self.generate_curl_command(stub, port);
            self.copy_to_clipboard(&curl_cmd);
            self.set_status(
                "Curl command copied to clipboard".to_string(),
                StatusLevel::Success,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::Stub;
    use serde_json::json;

    /// Stubs are deserialized rather than struct-literal'd so the tests exercise the same shape the
    /// admin API delivers, and do not have to track private fields.
    fn stub(predicates: serde_json::Value) -> Stub {
        serde_json::from_value(json!({ "predicates": predicates, "responses": [] }))
            .expect("stub fixture deserializes")
    }

    const PORT: u16 = 2525;

    #[test]
    fn get_with_a_query_predicate_becomes_a_query_string() {
        let app = crate::app::tests::make_test_app();
        let cmd = app.generate_curl_command(
            &stub(json!([{ "equals": { "method": "GET", "path": "/users", "query": { "page": "2" } } }])),
            PORT,
        );

        // GET is the default, so no `-X` — asserting the whole command is what pins that.
        assert_eq!(cmd, "curl -s \\\n  'http://localhost:2525/users?page=2'");
    }

    #[test]
    fn a_post_with_a_json_body_gets_a_method_a_content_type_and_a_body() {
        let app = crate::app::tests::make_test_app();
        let cmd = app.generate_curl_command(
            &stub(json!([{
                "equals": { "method": "POST", "path": "/orders", "body": { "sku": "ABC-1" } }
            }])),
            PORT,
        );

        assert_eq!(
            cmd,
            "curl -s \\\n  -X POST \\\n  -H 'Content-Type: application/json' \\\n  \
             -d '{\"sku\":\"ABC-1\"}' \\\n  'http://localhost:2525/orders'"
        );
    }

    #[test]
    fn a_header_predicate_is_emitted_as_a_dash_h_flag() {
        let app = crate::app::tests::make_test_app();
        let cmd = app.generate_curl_command(
            &stub(json!([{
                "equals": { "method": "GET", "path": "/p", "headers": { "X-Api-Key": "s3cret" } }
            }])),
            PORT,
        );

        assert_eq!(
            cmd,
            "curl -s \\\n  -H 'X-Api-Key: s3cret' \\\n  'http://localhost:2525/p'"
        );
    }

    /// An explicit Content-Type must not be duplicated by the JSON auto-detection.
    #[test]
    fn an_explicit_content_type_suppresses_the_automatic_one() {
        let app = crate::app::tests::make_test_app();
        let cmd = app.generate_curl_command(
            &stub(json!([{
                "equals": {
                    "method": "POST",
                    "path": "/p",
                    "headers": { "Content-Type": "application/vnd.api+json" },
                    "body": { "a": 1 }
                }
            }])),
            PORT,
        );

        assert_eq!(
            cmd.matches("Content-Type").count(),
            1,
            "the automatic header must stand down when one is declared: {cmd}"
        );
        assert!(cmd.contains("-H 'Content-Type: application/vnd.api+json'"));
    }

    /// The body is wrapped in single quotes, so a single quote inside it has to be closed,
    /// escaped and reopened — otherwise the emitted command is unrunnable shell.
    #[test]
    fn a_single_quote_in_the_body_is_escaped_for_the_shell() {
        let app = crate::app::tests::make_test_app();
        let cmd = app.generate_curl_command(
            &stub(json!([{ "equals": { "method": "POST", "path": "/p", "body": "it's here" } }])),
            PORT,
        );

        assert!(
            cmd.contains(r"-d 'it'\''s here'"),
            "the quote must be closed-escaped-reopened, got: {cmd}"
        );
        // A bare `it's` would terminate the quoted argument early; this is the regression that
        // makes the difference visible rather than merely different.
        assert!(
            !cmd.contains("-d 'it's here'"),
            "got unescaped quote: {cmd}"
        );
    }

    #[test]
    fn a_matches_predicate_becomes_a_sample_path_with_numbered_placeholders() {
        let app = crate::app::tests::make_test_app();

        assert_eq!(
            app.regex_to_sample_path("^/auto/dealers/[^/]+/dealer-customers/[^/]+$"),
            "/auto/dealers/{1}/dealer-customers/{2}"
        );
        // Anchors are stripped, `\d+` gets a sample value, and a pattern with no leading slash
        // still produces a path.
        assert_eq!(app.regex_to_sample_path(r"^/orders/\d+$"), "/orders/123");
        assert_eq!(app.regex_to_sample_path("orders"), "/orders");
    }

    #[test]
    fn jsonpath_parts_merge_into_one_nested_object() {
        let app = crate::app::tests::make_test_app();
        let merged = app.merge_jsonpath_bodies(&[
            ("$.user.id".to_string(), json!("123")),
            ("$.user.name".to_string(), json!("john")),
        ]);

        // Parsed rather than string-compared: key order is the serializer's business, the shape
        // is what this function promises.
        let parsed: serde_json::Value =
            serde_json::from_str(&merged).expect("merged body is valid JSON");
        assert_eq!(parsed, json!({ "user": { "id": "123", "name": "john" } }));
    }
}
