//! Issue #1629: a script sees a repeated query key, form key or header as an **array**, as
//! Mountebank does (`querystring.parse`, `headersMap.ofRaw`). One value stays a string.
//!
//! Every script surface reads the same request object: response `inject`, predicate `inject`,
//! `decorate` (JavaScript and Rhai), `shellTransform`'s `MB_REQUEST` and `_rift.script`. Templates
//! are not scripts: `${request.query.x}` still renders a repeated key comma-joined, which is what
//! `String(array)` gives in JavaScript.
//!
//! Each test sends `?q=1&q=2&one=x` with `X-A: 1`, `X-A: 2` and `X-B: solo`.

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

const QUERY: &str = "/p?q=1&q=2&one=x";

async fn create(manager: &ImposterManager, imposter: Value) -> u16 {
    // Created through the manager: the admin API refuses scripts without `--allowInjection`.
    let config = serde_json::from_value(imposter).expect("imposter config");
    manager.create_imposter(config).await.expect("create")
}

fn repeated(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    // `header` appends, so `X-A` goes out twice.
    request
        .header("X-A", "1")
        .header("X-A", "2")
        .header("X-B", "solo")
}

async fn get(port: u16) -> String {
    repeated(reqwest::Client::new().get(format!("http://127.0.0.1:{port}{QUERY}")))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body")
}

async fn body_of(imposter: Value) -> String {
    let manager = ImposterManager::new();
    let port = create(&manager, imposter).await;
    let body = get(port).await;
    let _ = manager.delete_imposter(port).await;
    body
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("a JSON body ({e}): {body:?}"))
}

/// What the scripts below return: the repeated and the single fields, in this order.
fn expected() -> Value {
    json!([["1", "2"], ["1", "2"], "x", "solo"])
}

const JS_ECHO: &str =
    "JSON.stringify([r.headers['X-A'], r.query.q, r.query.one, r.headers['x-b']])";

#[tokio::test]
async fn a_response_inject_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{"inject":
        format!("function (config) {{ var r = config.request; return {{ statusCode: 200, body: {JS_ECHO} }}; }}")
    }]}]}))
    .await;
    assert_eq!(json_of(&body), expected(), "{body:?}");
}

#[tokio::test]
async fn a_response_inject_sees_a_repeated_form_key_as_an_array() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{"inject":
            "function (config) { return { statusCode: 200, body: JSON.stringify(config.request.form) }; }"
        }]}]}),
    )
    .await;
    let body = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/p"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("f=1&g=solo&f=2")
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(port).await;
    assert_eq!(body, r#"{"f":["1","2"],"g":"solo"}"#);
}

#[tokio::test]
async fn a_predicate_inject_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [
        {"predicates": [{"inject": format!(
            "function (config) {{ var r = config.request; return {JS_ECHO} === '{}'; }}",
            expected()
        )}], "responses": [{"is": {"statusCode": 200, "body": "matched"}}]},
        {"responses": [{"is": {"statusCode": 200, "body": "fallback"}}]}
    ]}))
    .await;
    assert_eq!(body, "matched");
}

#[tokio::test]
async fn a_javascript_decorate_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{
        "is": {"statusCode": 200, "body": "orig"},
        "behaviors": [{"decorate": format!(
            "config => {{ var r = config.request; config.response.body = {JS_ECHO}; }}"
        )}]
    }]}]}))
    .await;
    assert_eq!(json_of(&body), expected(), "{body:?}");
}

#[tokio::test]
async fn a_rhai_decorate_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{
        "is": {"statusCode": 200, "body": "orig"},
        "behaviors": [{"decorate":
            "response.body = type_of(request.headers[\"X-A\"]) + \"|\" + type_of(request.query.q) + \"|\" + request.query.q[1] + \"|\" + request.query.one + \"|\" + request.headers[\"X-B\"];"
        }]
    }]}]}))
    .await;
    assert_eq!(body, "array|array|2|x|solo");
}

#[tokio::test]
async fn shell_transform_mb_request_carries_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{
        "is": {"statusCode": 200, "body": "orig"},
        // stdout is the response (#1589): `MB_REQUEST` as its JSON body.
        "behaviors": [{"shellTransform": "printf '{\"body\":%s}' \"$MB_REQUEST\" #"}]
    }]}]}))
    .await;
    let request = json_of(&body);
    assert_eq!(
        request["query"],
        json!({"q": ["1", "2"], "one": "x"}),
        "{request}"
    );
    assert_eq!(request["headers"]["X-A"], json!(["1", "2"]), "{request}");
    assert_eq!(request["headers"]["X-B"], json!("solo"), "{request}");
}

#[tokio::test]
async fn a_rhai_rift_script_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{"_rift": {"script": {
        "engine": "rhai",
        "code": "fn respond(ctx) { let r = ctx.request; http(200, [r.headers[\"x-a\"], r.query.q, r.query.one, r.header(\"X-B\")]) }"
    }}}]}]}))
    .await;
    assert_eq!(json_of(&body), expected(), "{body:?}");
}

#[tokio::test]
async fn a_javascript_rift_script_sees_a_repeated_header_and_query_key_as_an_array() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{"_rift": {"script": {
        "engine": "javascript",
        "code": "function respond(ctx) { var r = ctx.request; return http(200, [r.headers['x-a'], r.query.q, r.query.one, r.header('X-B')]); }"
    }}}]}]}))
    .await;
    assert_eq!(json_of(&body), expected(), "{body:?}");
}

#[tokio::test]
async fn a_template_still_renders_a_repeated_query_key_comma_joined() {
    let body = body_of(json!({"protocol": "http", "stubs": [{"responses": [{"is": {
        "statusCode": 200,
        "body": "${request.query.q}|${request.query.one}"
    }}]}]}))
    .await;
    assert_eq!(body, "1,2|x");
}
