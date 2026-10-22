//! Shell transform and decorate behaviors.

use super::request::RequestContext;
use std::collections::HashMap;

/// A response's `statusCode` as a behavior script sees and leaves it (issue #1558): a number, or a
/// string, which Mountebank keeps as written until it writes the response. A `statusCode` written
/// as a token is a string to `decorate` and `shellTransform`, as in Mountebank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Code(u16),
    Text(String),
}

impl From<u16> for ResponseStatus {
    fn from(code: u16) -> Self {
        ResponseStatus::Code(code)
    }
}

impl ResponseStatus {
    /// The status as JSON: a number, or the string.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            ResponseStatus::Code(code) => (*code).into(),
            ResponseStatus::Text(text) => text.as_str().into(),
        }
    }
}

/// Error executing a `decorate` behavior script.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecorateError {
    #[error("Decorate script error: {0}")]
    Rhai(String),
    #[error("JavaScript decorate error: {0}")]
    JavaScript(String),
    #[error("Could not parse JavaScript decorate function")]
    JsParseFailure,
    /// The script exceeded its wall-clock deadline on the bounded execution path (issue #476).
    #[error("decorate script timed out after {0}ms")]
    Timeout(u64),
}

/// What a `decorate` script left (issue #1567).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decorated {
    pub status_code: ResponseStatus,
    /// Exactly the script's headers, so a header it deleted is gone.
    pub headers: HashMap<String, String>,
    pub body: String,
    /// `Some` when the function returned an object, which replaces the response as in Mountebank
    /// (`behaviors.js:246-249`). It names the fields that object leaves out; they hold the built-in
    /// `200`, no headers and `""` here, and the pipeline completes them from the imposter's
    /// `defaultResponse`. `None` when the script changed the response it was handed.
    pub returned: Option<ReturnedOmits>,
}

/// The fields a decorate's returned object leaves out (issue #1567).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReturnedOmits {
    pub status_code: bool,
    pub headers: bool,
    pub body: bool,
}

/// Why a `shellTransform` command did not produce a response (issue #1589). The messages are
/// Mountebank's (`behaviors.js:186-201`), so a strict 500 says what Mountebank's does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShellTransformError {
    /// The shell could not be started at all.
    #[error("Could not run shell command: {0}")]
    Spawn(String),
    /// The command exited non-zero or was killed by a signal.
    #[error("Command failed: {command}\n{stderr}")]
    Failed { command: String, stderr: String },
    /// stdout was not JSON, empty included.
    #[error("Shell command returned invalid JSON: '{0}'")]
    InvalidJson(String),
    /// stdout was the JSON `null`, which Mountebank cannot read a response from.
    #[error("Shell command returned null, not a response")]
    Null,
    /// stdout was a JSON object that is not a response (a `statusCode` that is not a status, a
    /// header that is not a string).
    #[error("Shell command returned an invalid response: {0}")]
    InvalidResponse(String),
}

/// Mountebank's limit on the command line, in UTF-16 units (`behaviors.js:170`): at or above it the
/// two JSON arguments are dropped and the command runs alone.
pub const MAX_SHELL_COMMAND_LENGTH: usize = 2048;

/// `MB_REQUEST` for a `shellTransform`, as JSON text: Mountebank's `httpRequest.transform` keys. Each of
/// `requestFrom`, `ip` and `form` is absent when `None`, as `JSON.stringify` omits an undefined
/// value (issue #1451).
#[must_use]
pub fn shell_request_json(request: &RequestContext) -> String {
    use crate::util::JsKeyOrder;
    use serde::Serialize;

    // A struct, not a `serde_json::Map`: that map is a `BTreeMap` here and would sort the keys,
    // where Mountebank's `JSON.stringify` lists them in the order `httpRequest.transform` built
    // them, and `query` / `form` in the client's order (issue #1596).
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MbRequest<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_from: Option<&'a str>,
        method: &'a str,
        path: &'a str,
        query: JsKeyOrder<'a, String, foldhash::fast::RandomState>,
        headers: &'a HashMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ip: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        form: Option<JsKeyOrder<'a, String, foldhash::fast::RandomState>>,
    }

    let query = request.query_joined();
    let mb = MbRequest {
        request_from: request.request_from.as_deref(),
        method: &request.method,
        path: &request.path,
        query: JsKeyOrder(&query),
        headers: &request.headers,
        // `""` for a request without a body, as Mountebank's `JSON.stringify(request)` (#1365), and
        // no key for a body that did not decode, whose `undefined` it drops (#1584).
        body: (!request.body_undefined).then(|| request.body.as_deref().unwrap_or("")),
        ip: request.ip.as_deref(),
        form: request.form.as_ref().map(JsKeyOrder),
    };
    serde_json::to_string(&mb).expect("strings and string-keyed string maps always serialize")
}

/// One JSON text as one shell word: single-quoted, an embedded `'` written `'\''`. Mountebank does
/// not escape it (`behaviors.js:153-165`), so its command breaks on a `'`; where Mountebank's
/// works, the word is the same.
fn quote_for_shell(json: &str) -> String {
    format!("'{}'", json.replace('\'', r"'\''"))
}

/// The command line Mountebank runs (`behaviors.js:181-184`): the command, then the request and
/// the response JSON as two arguments, unless that line reaches [`MAX_SHELL_COMMAND_LENGTH`]
/// UTF-16 units, measured as Mountebank writes it (unescaped), when the command runs alone.
#[must_use]
pub fn shell_command_line(command: &str, request_json: &str, response_json: &str) -> String {
    let utf16 = |s: &str| s.encode_utf16().count();
    // `command 'request' 'response'`: two spaces and four quotes.
    let mountebank_len = utf16(command) + utf16(request_json) + utf16(response_json) + 6;
    if mountebank_len >= MAX_SHELL_COMMAND_LENGTH {
        command.to_string()
    } else {
        format!(
            "{command} {} {}",
            quote_for_shell(request_json),
            quote_for_shell(response_json)
        )
    }
}

/// Run a `shellTransform` command as Mountebank does (issue #1589).
///
/// `response` is the response as it stands (`statusCode`, `headers`, `body`). The command runs
/// under `sh -c` with `MB_REQUEST` and `MB_RESPONSE` set to the request and response JSON, and the
/// same two JSON texts appended as arguments ([`shell_command_line`]). Its stdout is parsed as
/// JSON and returned: the pipeline reads it as the new response. A non-zero exit, stdout that is
/// not JSON (empty included) and a JSON `null` are errors.
pub fn apply_shell_transform(
    command: &str,
    request: &RequestContext,
    response: &serde_json::Value,
) -> Result<serde_json::Value, ShellTransformError> {
    use std::process::Command;

    let request_json = shell_request_json(request);
    let response_json = response.to_string();
    let full_command = shell_command_line(command, &request_json, &response_json);

    let output = Command::new("sh")
        .arg("-c")
        .arg(&full_command)
        .env("MB_REQUEST", &request_json)
        .env("MB_RESPONSE", &response_json)
        .output()
        .map_err(|e| ShellTransformError::Spawn(e.to_string()))?;

    if !output.status.success() {
        return Err(ShellTransformError::Failed {
            command: full_command,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(serde_json::Value::Null) => Err(ShellTransformError::Null),
        Ok(value) => Ok(value),
        Err(_) => Err(ShellTransformError::InvalidJson(stdout.into_owned())),
    }
}

/// Whether a `decorate` script is a JavaScript function value (issue #1567): `function`, an arrow
/// (`(<params>) =>` or `<identifier> =>`), either optionally `async`. Start-anchored on the trimmed
/// script. Mountebank calls any of them as `fn(config, response, logger)`, whatever the parameters
/// are named. Rhai cannot start a script this way: its closures are `|x|`, and `=>` appears only
/// inside a `switch`.
#[must_use]
pub fn is_js_function_decorate(script: &str) -> bool {
    let t = script.trim_start();
    if is_function_head(t) {
        return true;
    }
    // `async` is a keyword only when a function head follows it; `async => 1` is an arrow whose
    // parameter is named `async`, which the check above already took.
    t.strip_prefix("async")
        .filter(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '('))
        .is_some_and(|rest| is_function_head(rest.trim_start()))
}

fn is_function_head(t: &str) -> bool {
    t.starts_with("function") || is_arrow_head(t)
}

/// `(<anything without ')'>) =>` or `<identifier> =>`.
fn is_arrow_head(t: &str) -> bool {
    let rest = if let Some(inner) = t.strip_prefix('(') {
        match inner.find(')') {
            Some(close) => &inner[close + 1..],
            None => return false,
        }
    } else {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let end = t.find(|c: char| !is_ident(c)).unwrap_or(t.len());
        if end == 0 || t.starts_with(|c: char| c.is_ascii_digit()) {
            return false;
        }
        &t[end..]
    };
    rest.trim_start().starts_with("=>")
}

/// Whether the engine runs a `decorate` script as JavaScript rather than Rhai: a function value
/// ([`is_js_function_decorate`]) or a bare body in the `config` convention
/// ([`is_js_config_decorate`]), which the engine wraps in `function(config) { ... }`.
/// `rift-lint` carries a copy of this rule, pinned by
/// `crates/rift-http-proxy/tests/issue_1156_decorate_routing.rs`.
#[must_use]
pub fn is_javascript_decorate(script: &str) -> bool {
    is_js_function_decorate(script) || is_js_config_decorate(script)
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
            // `get` is `None` when the `}` precedes the `{`: no body to strip.
            (Some(open), Some(close)) => script.get(open + 1..close).unwrap_or(script),
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
    response_status: &ResponseStatus,
    response_headers: &mut HashMap<String, String>,
) -> Result<(String, ResponseStatus), DecorateError> {
    use rhai::{Dynamic, Map, Scope};

    let engine = crate::scripting::rift_rhai_engine();
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
    for (k, v) in request.query_joined() {
        query_map.insert(k.into(), Dynamic::from(v));
    }
    req_map.insert("query".into(), Dynamic::from(query_map));

    let mut headers_map = Map::new();
    for (k, v) in &request.headers {
        headers_map.insert(k.clone().into(), Dynamic::from(v.clone()));
    }
    req_map.insert("headers".into(), Dynamic::from(headers_map));

    // Create response map for Rhai
    let mut resp_map = Map::new();
    let status = match response_status {
        ResponseStatus::Code(code) => Dynamic::from(i64::from(*code)),
        ResponseStatus::Text(text) => Dynamic::from(text.clone()),
    };
    resp_map.insert("statusCode".into(), status);
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
            match scope.get_value::<Map>("response") {
                Some(response) => {
                    let new_body = response
                        .get("body")
                        .and_then(|v| v.clone().try_cast::<String>())
                        .unwrap_or_else(|| response_body.to_string());

                    // A number or a string, as the script left it; anything else keeps the
                    // status it was given.
                    let new_status = response
                        .get("statusCode")
                        .and_then(|v| {
                            v.clone()
                                .try_cast::<i64>()
                                .map(|s| ResponseStatus::Code(s as u16))
                                .or_else(|| {
                                    v.clone().try_cast::<String>().map(ResponseStatus::Text)
                                })
                        })
                        .unwrap_or_else(|| response_status.clone());

                    // Update headers from response map
                    if let Some(headers) = response.get("headers")
                        && let Some(headers_map) = headers.clone().try_cast::<Map>()
                    {
                        for (k, v) in headers_map {
                            if let Some(value) = v.try_cast::<String>() {
                                response_headers.insert(k.to_string(), value);
                            }
                        }
                    }

                    Ok((new_body, new_status))
                }
                _ => Ok((response_body.to_string(), response_status.clone())),
            }
        }
        Err(e) => Err(DecorateError::Rhai(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &str) -> serde_json::Value {
        serde_json::json!({ "statusCode": 200, "headers": {}, "body": body })
    }

    /// The `body` of the response a command printed.
    fn shell_body(command: &str, request: &RequestContext, body: &str) -> String {
        let out = apply_shell_transform(command, request, &response(body)).expect("runs");
        out["body"].as_str().expect("a string body").to_string()
    }

    #[test]
    fn test_apply_shell_transform_echo() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/test".to_string(),
            body: Some(r#"{"test": "data"}"#.to_string()),
            ..Default::default()
        };
        // The trailing `#` discards the two appended JSON arguments, as Mountebank configs do.
        let result = apply_shell_transform(
            r#"echo '{"body": "hello world"}' #"#,
            &request,
            &response("original body"),
        );
        assert_eq!(
            result,
            Ok(serde_json::json!({ "body": "hello world" })),
            "stdout is the parsed JSON response"
        );
    }

    /// Issue #1451: a request with no client socket and no form leaves `requestFrom`, `ip` and
    /// `form` out of `MB_REQUEST`, as `JSON.stringify` omits Mountebank's undefined ones.
    #[test]
    fn mb_request_omits_absent_client_fields() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/p".to_string(),
            ..Default::default()
        };
        let output = apply_shell_transform(
            r#"printf '{"body":%s}' "$MB_REQUEST" #"#,
            &request,
            &response("b"),
        )
        .expect("shell transform runs");
        let fields = output["body"].as_object().expect("MB_REQUEST is an object");
        for key in ["requestFrom", "ip", "form"] {
            assert!(!fields.contains_key(key), "{key} must be absent: {output}");
        }
        assert_eq!(fields.get("method"), Some(&serde_json::json!("GET")));
    }

    /// Issue #1365: `MB_REQUEST` carries `"body": ""` for a request without a body, as Mountebank's
    /// `JSON.stringify(request)` does, not `null`.
    #[test]
    fn mb_request_body_is_empty_string_when_absent() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/nobody".to_string(),
            body: None,
            ..Default::default()
        };
        let out = apply_shell_transform(
            r#"printf '{"body":%s}' "$MB_REQUEST" #"#,
            &request,
            &response("x"),
        )
        .expect("shell transform runs");
        assert_eq!(out["body"]["body"], serde_json::json!(""));
    }

    /// Issue #1596: `MB_REQUEST` lists its keys as Mountebank's `JSON.stringify(request)` does, and
    /// `query` / `form` in the client's order. A `serde_json::Map` here sorted all three.
    #[test]
    fn mb_request_keeps_mountebank_key_order() {
        let query = crate::util::parse_query_string("b=1&10=x&2=y&a=3");
        let form = crate::util::parse_query_string("z=1&y=2");
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/p".to_string(),
            query,
            headers: HashMap::new(),
            body: Some("z=1&y=2".to_string()),
            request_from: Some("127.0.0.1:1".to_string()),
            ip: Some("127.0.0.1".to_string()),
            form: Some(crate::util::comma_joined(&form)),
            ..Default::default()
        };
        assert_eq!(
            shell_request_json(&request),
            r#"{"requestFrom":"127.0.0.1:1","method":"POST","path":"/p","query":{"2":"y","10":"x","b":"1","a":"3"},"headers":{},"body":"z=1&y=2","ip":"127.0.0.1","form":{"z":"1","y":"2"}}"#
        );
    }

    /// Issue #1584: a body that did not decode is Mountebank's `undefined`, which
    /// `JSON.stringify(request)` drops, so `MB_REQUEST` has no `body` key.
    #[test]
    fn mb_request_has_no_body_key_when_the_body_did_not_decode() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/gz".to_string(),
            body_undefined: true,
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&shell_request_json(&request)).expect("json");
        assert!(json.get("body").is_none(), "{json}");
        assert_eq!(json["method"], serde_json::json!("POST"));
    }

    /// Issue #1589: the two appended arguments are the request and the response JSON, one word
    /// each, and a `'` in either survives the quoting.
    #[test]
    fn the_appended_arguments_are_the_request_and_response_json() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/it's".to_string(),
            body: Some("a 'quoted' $HOME `x` body".to_string()),
            ..Default::default()
        };
        let resp = serde_json::json!({
            "statusCode": 201, "headers": { "X-A": "it's" }, "body": "o'k \"q\""
        });
        let out = apply_shell_transform(
            r#"f() { printf '{"body":[%s,%s,"%s"]}' "$1" "$2" "$#"; }; f"#,
            &request,
            &resp,
        )
        .expect("runs");
        let body = &out["body"];
        let expected: serde_json::Value =
            serde_json::from_str(&shell_request_json(&request)).expect("json");
        assert_eq!(body[0], expected);
        assert_eq!(body[1], resp);
        assert_eq!(body[2], "2", "exactly two arguments");
    }

    /// Issue #1589: `MB_RESPONSE` is the whole response, headers included.
    #[test]
    fn mb_response_is_the_whole_response() {
        let resp = serde_json::json!({
            "statusCode": "${code}", "headers": { "X-A": "1", "Set-Cookie": ["a", "b"] }, "body": "o"
        });
        let out = apply_shell_transform(
            r#"printf '{"body":%s}' "$MB_RESPONSE" #"#,
            &RequestContext::default(),
            &resp,
        )
        .expect("runs");
        assert_eq!(out["body"], resp);
    }

    /// Issue #1589: a command line at or above 2048 UTF-16 units runs without the arguments, as
    /// Mountebank's does; one below keeps them.
    #[test]
    fn a_long_command_line_drops_the_arguments() {
        let padding = format!(": {};", "x".repeat(MAX_SHELL_COMMAND_LENGTH));
        let command = format!(r#"{padding} f() {{ printf '{{"body":"%s"}}' "$#"; }}; f"#);
        assert_eq!(shell_body(&command, &RequestContext::default(), "b"), "0");
        let short = r#"f() { printf '{"body":"%s"}' "$#"; }; f"#;
        assert_eq!(shell_body(short, &RequestContext::default(), "b"), "2");
    }

    /// The boundary, measured as Mountebank measures `fullCommand.length`: unescaped, in UTF-16
    /// units, so a character outside the BMP counts twice.
    #[test]
    fn the_length_limit_is_mountebanks() {
        let (req, resp) = ("{}", "{}");
        // `cmd '{}' '{}'` is the command plus 10 units.
        let fits = "c".repeat(MAX_SHELL_COMMAND_LENGTH - 11);
        assert_eq!(
            shell_command_line(&fits, req, resp),
            format!("{fits} '{{}}' '{{}}'")
        );
        let reaches = "c".repeat(MAX_SHELL_COMMAND_LENGTH - 10);
        assert_eq!(shell_command_line(&reaches, req, resp), reaches);
        // U+1F600 is two UTF-16 units, so this one reaches the limit too.
        let astral = format!("{}\u{1F600}", "c".repeat(MAX_SHELL_COMMAND_LENGTH - 12));
        assert_eq!(shell_command_line(&astral, req, resp), astral);
        // A `'` is escaped as `'\''`, but the limit counts Mountebank's unescaped one.
        let with_quote = "{\"a\":\"'\"}";
        assert_eq!(
            shell_command_line("c", with_quote, "{}"),
            r#"c '{"a":"'\''"}' '{}'"#
        );
        let last_fit = "c".repeat(MAX_SHELL_COMMAND_LENGTH - 1 - 6 - with_quote.len() - 2);
        assert_ne!(shell_command_line(&last_fit, with_quote, "{}"), last_fit);
    }

    /// Issue #1589: what stdout is decides the outcome. Any non-null JSON is returned; `null`,
    /// text, empty output and a non-zero exit are errors carrying Mountebank's message.
    #[test]
    fn each_stdout_class() {
        let run = |command: &str| {
            apply_shell_transform(command, &RequestContext::default(), &response("b"))
        };
        assert_eq!(
            run(r#"printf '{"body":"x"}' #"#),
            Ok(serde_json::json!({ "body": "x" }))
        );
        assert_eq!(run("printf ' 3\n' #"), Ok(serde_json::json!(3)));
        assert_eq!(run("printf null #"), Err(ShellTransformError::Null));
        assert_eq!(
            run("echo hello #"),
            Err(ShellTransformError::InvalidJson("hello\n".to_string()))
        );
        assert_eq!(
            run("true"),
            Err(ShellTransformError::InvalidJson(String::new()))
        );
        let failed = run("echo bad >&2; exit 3 #").expect_err("non-zero exit");
        assert!(
            matches!(&failed, ShellTransformError::Failed { stderr, .. } if stderr == "bad\n"),
            "{failed:?}"
        );
        assert!(
            failed
                .to_string()
                .starts_with("Command failed: echo bad >&2; exit 3 # '"),
            "{failed}"
        );
    }

    #[test]
    fn test_apply_decorate_modify_body() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: Default::default(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        // Script that modifies the response body
        let script = r#"
            response.body = "modified body";
        "#;

        let result = apply_decorate(
            script,
            &request,
            "original body",
            &ResponseStatus::Code(200),
            &mut headers,
        );
        assert!(result.is_ok());
        let (body, status) = result.unwrap();
        assert_eq!(body, "modified body");
        assert_eq!(status, ResponseStatus::Code(200));
    }

    #[test]
    fn test_apply_decorate_modify_status() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: Default::default(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let mut headers = HashMap::new();

        // Script that modifies the response status
        let script = r#"
            response.statusCode = 201;
        "#;

        let result = apply_decorate(
            script,
            &request,
            "body",
            &ResponseStatus::Code(200),
            &mut headers,
        );
        assert!(result.is_ok());
        let (body, status) = result.unwrap();
        assert_eq!(body, "body");
        assert_eq!(status, ResponseStatus::Code(201));
    }

    #[test]
    fn test_apply_decorate_access_request() {
        let request = RequestContext {
            method: "POST".to_string(),
            path: "/users".to_string(),
            query: Default::default(),
            headers: HashMap::new(),
            body: Some(r#"{"name": "Alice"}"#.to_string()),
            ..Default::default()
        };

        let mut headers = HashMap::new();

        // Script that uses request data in response
        let script = r#"
            response.body = "Method: " + request.method + ", Path: " + request.path;
        "#;

        let result = apply_decorate(
            script,
            &request,
            "original",
            &ResponseStatus::Code(200),
            &mut headers,
        );
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
            query: Default::default(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "text/plain".to_string());

        // Script that modifies response headers
        let script = r#"
            response.headers["x-custom"] = "custom-value";
        "#;

        let result = apply_decorate(
            script,
            &request,
            "body",
            &ResponseStatus::Code(200),
            &mut headers,
        );
        assert!(result.is_ok());
        assert_eq!(headers.get("x-custom"), Some(&"custom-value".to_string()));
    }

    #[test]
    fn test_apply_decorate_script_error() {
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: Default::default(),
            headers: HashMap::new(),
            body: None,
            ..Default::default()
        };

        let mut headers = HashMap::new();

        // Invalid script with syntax error
        let script = "this is not valid rhai {{{";

        let result = apply_decorate(
            script,
            &request,
            "body",
            &ResponseStatus::Code(200),
            &mut headers,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Decorate script error")
        );
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

    // Issue #1384: each spelling is detected on its own. The bodies deliberately avoid
    // `config.response.`, which would otherwise satisfy the check whatever the prefix tests do.
    #[test]
    fn every_config_spelling_is_detected_on_its_own() {
        for script in [
            "config => {}",
            "config=>{}",
            "(config) => {}",
            "(config)=>{}",
            "function(config) {}",
            "function (config) {}",
            "var p = config.request.path;",
            "config.response.body = 1;",
        ] {
            assert!(is_js_config_decorate(script), "{script}");
        }
        for script in ["response.body = 1;", "function(request, response) {}"] {
            assert!(!is_js_config_decorate(script), "{script}");
        }
    }

    // Issue #1567: every function form is recognised, whatever its parameters are named; Rhai is
    // not.
    #[test]
    fn every_function_form_is_a_javascript_function_decorate() {
        for script in [
            "function (request, response) {}",
            "function(config) {}",
            "async function (r) {}",
            "(request, response) => {}",
            "(req, res)=>{}",
            "() => 1",
            "({ response }) => {}",
            "request => {}",
            "$r => {}",
            "_ => {}",
            "async (request, response) => {}",
            "async(r) => {}",
            "async config => {}",
            "async => 1",
            "  \n config => {}",
        ] {
            assert!(is_js_function_decorate(script), "{script:?}");
            assert!(is_javascript_decorate(script), "{script:?}");
        }
        for script in [
            "",
            "response.body = 1;",
            "(1 + 2) * 3",
            "(a = f()) => 1",
            "1 => 2",
            "let f = |x| x + 1;",
            "switch x { 1 => () }",
            "asyncfn(1)",
            "async x",
            "config.response.body = 'x';",
        ] {
            assert!(!is_js_function_decorate(script), "{script:?}");
        }
        assert!(is_javascript_decorate("config.response.body = 'x';"));
        assert!(!is_javascript_decorate("response.body = 1;"));
    }

    // Issue #1384: a `}` before the first `{` leaves no wrapper body to strip, so the whole
    // script is rewritten as-is instead of slicing a reversed range.
    #[test]
    fn a_script_whose_closing_brace_precedes_its_opening_one_is_not_stripped() {
        assert_eq!(
            rewrite_js_config_to_rhai("config => config.response.body = '}{';").trim(),
            r#"config => response.body = "}{";"#
        );
    }
}
