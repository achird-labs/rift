//! Shell transform and decorate behaviors.

use super::request::RequestContext;
use std::collections::HashMap;

/// Execute shell transform command
/// The command receives MB_REQUEST and MB_RESPONSE environment variables
/// and should output the transformed response body to stdout
pub fn apply_shell_transform(
    command: &str,
    request: &RequestContext,
    response_body: &str,
    response_status: u16,
) -> Result<String, std::io::Error> {
    use std::process::Command;

    // Serialize request to JSON for MB_REQUEST
    let request_json = serde_json::json!({
        "method": request.method,
        "path": request.path,
        "query": request.query,
        "headers": request.headers,
        "body": request.body,
    });

    // Serialize response to JSON for MB_RESPONSE
    let response_json = serde_json::json!({
        "statusCode": response_status,
        "body": response_body,
    });

    // Execute command with environment variables
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("MB_REQUEST", request_json.to_string())
        .env("MB_RESPONSE", response_json.to_string())
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(std::io::Error::other(format!(
            "Shell transform failed: {stderr}"
        )))
    }
}

/// Detect Mountebank's JavaScript `config =>` decorate calling convention (issue #191): an arrow or
/// `function(config)` whose body reads/writes `config.request`/`config.response`. These can't run in
/// Rhai as-is (arrow syntax + the `config` variable model), so they need rewriting/routing.
pub fn is_js_config_decorate(script: &str) -> bool {
    let t = script.trim_start();
    t.starts_with("config =>")
        || t.starts_with("config=>")
        || t.starts_with("(config) =>")
        || t.starts_with("(config)=>")
        || t.starts_with("function(config)")
        || t.starts_with("function (config)")
        || t.contains("config.request.")
        || t.contains("config.response.")
}

static JSON_FN_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

/// Rewrite a simple JS `config` decorate script into Rhai (issue #191, Part A). The body's field
/// reads/writes on `config.request`/`config.response` map onto the `request`/`response` maps that
/// `apply_decorate` already exposes. This covers inline scripts that do simple field access; full
/// JavaScript (require/closures, JSON parsing of objects, spreads) is the separate JS-engine route.
pub fn rewrite_js_config_to_rhai(script: &str) -> String {
    // Strip a `config =>` / `function(config)` wrapper down to the function body.
    let body = if script.contains("=>") || script.trim_start().starts_with("function") {
        match (script.find('{'), script.rfind('}')) {
            (Some(open), Some(close)) if close > open => &script[open + 1..close],
            _ => script,
        }
    } else {
        script
    };

    let mut rhai = body.trim().to_string();
    rhai = rhai.replace("config.request.", "request.");
    rhai = rhai.replace("config.response.", "response.");
    // JS declarations → Rhai `let`.
    rhai = rhai.replace("const ", "let ").replace("var ", "let ");
    // Best-effort: JSON.parse(x) / JSON.stringify(x) with a simple (non-nested) arg → the arg
    // itself — the body is already a string in Rhai scope, so simple field access needs no JSON.
    let json_re = JSON_FN_RE
        .get_or_init(|| regex::Regex::new(r"JSON\.(?:parse|stringify)\(([^()]*)\)").unwrap());
    rhai = json_re.replace_all(&rhai, "$1").to_string();
    // Mountebank scripts commonly use single-quoted strings; Rhai needs double quotes.
    rhai.replace('\'', "\"")
}

/// Apply decorate behavior using Rhai script (Mountebank-compatible)
/// The script can access and modify `request` and `response` variables
pub fn apply_decorate(
    script: &str,
    request: &RequestContext,
    response_body: &str,
    response_status: u16,
    response_headers: &mut HashMap<String, String>,
) -> Result<(String, u16), String> {
    use rhai::{Dynamic, Engine, Map, Scope};

    let engine = Engine::new();
    let mut scope = Scope::new();

    // Create request map for Rhai
    let mut req_map = Map::new();
    req_map.insert("method".into(), Dynamic::from(request.method.clone()));
    req_map.insert("path".into(), Dynamic::from(request.path.clone()));
    req_map.insert(
        "body".into(),
        Dynamic::from(request.body.clone().unwrap_or_default()),
    );

    let mut query_map = Map::new();
    for (k, v) in &request.query {
        query_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    req_map.insert("query".into(), Dynamic::from(query_map));

    let mut headers_map = Map::new();
    for (k, v) in &request.headers {
        headers_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    req_map.insert("headers".into(), Dynamic::from(headers_map));

    // Create response map for Rhai
    let mut resp_map = Map::new();
    resp_map.insert("statusCode".into(), Dynamic::from(response_status as i64));
    resp_map.insert("body".into(), Dynamic::from(response_body.to_string()));

    let mut resp_headers_map = Map::new();
    for (k, v) in response_headers.iter() {
        resp_headers_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    resp_map.insert("headers".into(), Dynamic::from(resp_headers_map));

    scope.push("request", req_map);
    scope.push("response", resp_map);

    // Execute the decoration script
    match engine.eval_with_scope::<Dynamic>(&mut scope, script) {
        Ok(_) => {
            // Extract modified response from scope
            if let Some(response) = scope.get_value::<Map>("response") {
                let new_body = response
                    .get("body")
                    .and_then(|v| v.clone().try_cast::<String>())
                    .unwrap_or_else(|| response_body.to_string());

                let new_status = response
                    .get("statusCode")
                    .and_then(|v| v.clone().try_cast::<i64>())
                    .map(|s| s as u16)
                    .unwrap_or(response_status);

                // Update headers from response map
                if let Some(headers) = response.get("headers") {
                    if let Some(headers_map) = headers.clone().try_cast::<Map>() {
                        for (k, v) in headers_map {
                            if let Some(value) = v.try_cast::<String>() {
                                response_headers.insert(k.to_string(), value);
                            }
                        }
                    }
                }

                Ok((new_body, new_status))
            } else {
                Ok((response_body.to_string(), response_status))
            }
        }
        Err(e) => Err(format!("Decorate script error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_shell_transform_echo() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some(r#"{"test": "data"}"#.to_string()),
        };

        // Simple echo command that outputs a fixed string
        let result = apply_shell_transform("echo 'hello world'", &request, "original body", 200);
        assert!(result.is_ok(), "Shell transform should succeed");
        assert!(
            result.unwrap().contains("hello world"),
            "Shell transform should output echo result"
        );
    }

    #[test]
    fn test_apply_shell_transform_with_env_vars() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/users/123".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        // Command that outputs the MB_REQUEST env var (which contains JSON)
        let result = apply_shell_transform("echo $MB_REQUEST", &request, "test body", 200);
        assert!(result.is_ok(), "Shell transform should succeed");

        let output = result.unwrap();
        // The output should contain parts of the request context
        assert!(
            output.contains("GET") || output.contains("method"),
            "MB_REQUEST should contain request method"
        );
    }

    #[test]
    fn test_apply_decorate_modify_body() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        // Script that modifies the response body
        let script = r#"
            response.body = "modified body";
        "#;

        let result = apply_decorate(script, &request, "original body", 200, &mut headers);
        assert!(result.is_ok());
        let (body, status) = result.unwrap();
        assert_eq!(body, "modified body");
        assert_eq!(status, 200);
    }

    #[test]
    fn test_apply_decorate_modify_status() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        let mut headers = HashMap::new();

        // Script that modifies the response status
        let script = r#"
            response.statusCode = 201;
        "#;

        let result = apply_decorate(script, &request, "body", 200, &mut headers);
        assert!(result.is_ok());
        let (body, status) = result.unwrap();
        assert_eq!(body, "body");
        assert_eq!(status, 201);
    }

    #[test]
    fn test_apply_decorate_access_request() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/users".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Some(r#"{"name": "Alice"}"#.to_string()),
        };

        let mut headers = HashMap::new();

        // Script that uses request data in response
        let script = r#"
            response.body = "Method: " + request.method + ", Path: " + request.path;
        "#;

        let result = apply_decorate(script, &request, "original", 200, &mut headers);
        assert!(result.is_ok());
        let (body, _status) = result.unwrap();
        assert!(body.contains("Method: POST"));
        assert!(body.contains("Path: /users"));
    }

    #[test]
    fn test_apply_decorate_modify_headers() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "text/plain".to_string());

        // Script that modifies response headers
        let script = r#"
            response.headers["x-custom"] = "custom-value";
        "#;

        let result = apply_decorate(script, &request, "body", 200, &mut headers);
        assert!(result.is_ok());
        assert_eq!(headers.get("x-custom"), Some(&"custom-value".to_string()));
    }

    #[test]
    fn test_apply_decorate_script_error() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        };

        let mut headers = HashMap::new();

        // Invalid script with syntax error
        let script = "this is not valid rhai {{{";

        let result = apply_decorate(script, &request, "body", 200, &mut headers);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Decorate script error"));
    }

    // Issue #191: JS `config =>` decorate convention detection + rewrite.
    #[test]
    fn is_js_config_decorate_detects_convention() {
        assert!(is_js_config_decorate(
            "config => { config.response.body = 'x'; }"
        ));
        assert!(is_js_config_decorate(
            "(config) => { config.response.body = 'x'; }"
        ));
        assert!(is_js_config_decorate(
            "function(config) { config.response.body = 'x'; }"
        ));
        assert!(is_js_config_decorate("config.response.statusCode = 404;"));
        // Plain Rhai and the 2-arg `function(request, response)` JS form are NOT this convention.
        assert!(!is_js_config_decorate("response.body = request.body;"));
        assert!(!is_js_config_decorate(
            "function(request, response) { response.body = 'x'; }"
        ));
    }

    #[test]
    fn rewrite_js_config_maps_to_rhai() {
        assert_eq!(
            rewrite_js_config_to_rhai("config => { config.response.body = 'hello'; }").trim(),
            r#"response.body = "hello";"#
        );
        assert_eq!(
            rewrite_js_config_to_rhai("config => { config.response.body = config.request.body; }")
                .trim(),
            "response.body = request.body;"
        );
    }

    #[test]
    fn rewrite_js_config_simple_json_helpers_drop_to_arg() {
        // Best-effort: a non-nested JSON.parse/stringify arg becomes the arg (the body is already
        // a string in Rhai scope). Nested-paren args are left for the JS-engine route (Part B).
        assert_eq!(
            rewrite_js_config_to_rhai(
                "config => { config.response.body = JSON.stringify(config.request.body); }"
            )
            .trim(),
            "response.body = request.body;"
        );
    }
}
