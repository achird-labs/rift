//! Issue #1451: every script surface gets Mountebank's whole request object. Mountebank's
//! `httpRequest.transform` adds `requestFrom` (`address:port`), `ip` and, for a non-empty urlencoded
//! body, `form`; response `inject`, predicate `inject`, generator `inject`, `decorate` and
//! `shellTransform`'s `MB_REQUEST` all receive it. Rift's values are its own transport view, the
//! strings the `requestFrom`/`ip` predicates match. A host-less imposter listens dual-stack, as in
//! Mountebank (#1514), so an IPv4 client is `::ffff:127.0.0.1` and its `requestFrom` is written
//! `[::ffff:127.0.0.1]:<port>`.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

const FORM: &str = "application/x-www-form-urlencoded";

async fn create(manager: &ImposterManager, imposter: Value) -> u16 {
    // Created through the manager: the admin API refuses scripts without `--allowInjection`.
    let config = serde_json::from_value(imposter).expect("imposter config");
    manager.create_imposter(config).await.expect("create")
}

async fn post_form(port: u16, body: &'static str) -> String {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/submit"))
        .header("Content-Type", FORM)
        .body(body)
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body")
}

async fn get(port: u16) -> String {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/submit"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body")
}

/// `::ffff:127.0.0.1|[::ffff:127.0.0.1]:<digits>|<rest>` → `<rest>`, or a panic naming what didn't
/// match.
fn after_client_fields(body: &str) -> &str {
    let rest = body
        .strip_prefix("::ffff:127.0.0.1|[::ffff:127.0.0.1]:")
        .unwrap_or_else(|| panic!("ip and requestFrom must lead the body: {body:?}"));
    let (port, rest) = rest
        .split_once('|')
        .unwrap_or_else(|| panic!("three fields expected: {body:?}"));
    assert!(
        !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        "requestFrom must end in the client port: {body:?}"
    );
    rest
}

#[tokio::test]
async fn a_response_inject_sees_request_from_ip_and_form() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{"inject":
            "function (config) { var request = config.request; return { statusCode: 200, body: [request.ip, request.requestFrom, typeof request.form, (request.form || {}).a].join('|') }; }"
        }]}]}),
    )
    .await;

    let body = post_form(port, "a=1").await;
    assert_eq!(after_client_fields(&body), "object|1", "{body:?}");

    let body = get(port).await;
    assert_eq!(
        after_client_fields(&body),
        "undefined|",
        "a GET has no form: {body:?}"
    );

    let body = post_form(port, "").await;
    assert_eq!(
        after_client_fields(&body),
        "undefined|",
        "an empty urlencoded body has no form: {body:?}"
    );

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_predicate_inject_sees_request_from_ip_and_form() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [
            {"predicates": [{"inject":
                "function (config) { var r = config.request; return /^::ffff:127\\.0\\.0\\.1\\|\\[::ffff:127\\.0\\.0\\.1\\]:\\d+$/.test(r.ip + '|' + r.requestFrom) && (r.form || {}).a === '1'; }"
            }], "responses": [{"is": {"statusCode": 200, "body": "matched"}}]},
            {"predicates": [{"inject":
                "function (config) { return typeof config.request.form === 'undefined'; }"
            }], "responses": [{"is": {"statusCode": 200, "body": "no-form"}}]},
            {"responses": [{"is": {"statusCode": 200, "body": "fallback"}}]}
        ]}),
    )
    .await;

    assert_eq!(post_form(port, "a=1").await, "matched");
    assert_eq!(post_form(port, "a=2").await, "fallback");
    assert_eq!(get(port).await, "no-form", "a GET has no form");
    assert_eq!(
        post_form(port, "").await,
        "no-form",
        "an empty urlencoded body has no form"
    );

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_generator_inject_records_the_client_ip_and_request_from() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .bind()
        .await
        .expect("admin API binds");
    let admin_url = format!("http://{}", admin.local_addr());

    let upstream = create(
        &manager,
        json!({"protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "up"}}]}]}),
    )
    .await;
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{"proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{"inject":
                "function (config) { if (typeof config.request.requestFrom !== 'string') { throw new Error('requestFrom is ' + typeof config.request.requestFrom); } return [{ equals: { ip: config.request.ip } }, { equals: { requestFrom: config.request.requestFrom } }]; }"}]
        }}]}]}),
    )
    .await;

    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/a"))
        .send()
        .await
        .expect("request");
    let generator_error = response
        .headers()
        .get("x-rift-generator-error")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    assert_eq!(generator_error, None, "the generation must succeed");
    assert_eq!(response.text().await.expect("body"), "up");

    let printed: Value = reqwest::Client::new()
        .get(format!("{admin_url}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let stubs = printed["stubs"].as_array().expect("stubs");
    assert_eq!(
        stubs.len(),
        2,
        "one recording in front of the proxy: {stubs:?}"
    );
    let predicates = stubs[0]["predicates"].as_array().expect("predicates");
    assert_eq!(predicates.len(), 2, "{stubs:?}");
    assert_eq!(
        predicates[0],
        json!({"equals": {"ip": "::ffff:127.0.0.1"}}),
        "{stubs:?}"
    );
    let request_from = predicates[1]["equals"]["requestFrom"]
        .as_str()
        .unwrap_or_else(|| panic!("a recorded requestFrom string: {stubs:?}"));
    assert_eq!(
        after_client_fields(&format!("::ffff:127.0.0.1|{request_from}|")),
        "",
        "{stubs:?}"
    );

    let _ = manager.delete_imposter(port).await;
    let _ = manager.delete_imposter(upstream).await;
}

async fn decorated_body(decorate: &str) -> String {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "orig"},
            "behaviors": [{"decorate": decorate}]
        }]}]}),
    )
    .await;
    let body = get(port).await;
    let _ = manager.delete_imposter(port).await;
    body
}

#[tokio::test]
async fn a_config_style_decorate_sees_the_client_ip_and_request_from() {
    let body = decorated_body(
        "config => { config.response.body = [config.request.ip, config.request.requestFrom, ''].join('|'); }",
    )
    .await;
    assert_eq!(after_client_fields(&body), "", "{body:?}");
}

#[tokio::test]
async fn a_function_style_decorate_sees_the_client_ip_and_request_from() {
    let body = decorated_body(
        "function (request, response) { response.body = [request.ip, request.requestFrom, ''].join('|'); }",
    )
    .await;
    assert_eq!(after_client_fields(&body), "", "{body:?}");
}

#[tokio::test]
async fn shell_transform_mb_request_carries_request_from_ip_and_form() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{
            "is": {"statusCode": 200, "body": "orig"},
            "behaviors": [{"shellTransform": "printf '%s' \"$MB_REQUEST\""}]
        }]}]}),
    )
    .await;

    let posted: Value = serde_json::from_str(&post_form(port, "a=1").await).expect("MB_REQUEST");
    assert_eq!(posted["ip"], json!("::ffff:127.0.0.1"), "{posted}");
    let request_from = posted["requestFrom"].as_str().unwrap_or_default();
    let client_port = request_from
        .strip_prefix("[::ffff:127.0.0.1]:")
        .unwrap_or_default();
    assert!(
        !client_port.is_empty() && client_port.bytes().all(|b| b.is_ascii_digit()),
        "requestFrom is address:port: {posted}"
    );
    assert_eq!(posted["form"], json!({"a": "1"}), "{posted}");

    let got: Value = serde_json::from_str(&get(port).await).expect("MB_REQUEST");
    assert_eq!(got["ip"], json!("::ffff:127.0.0.1"), "{got}");
    assert!(got["requestFrom"].is_string(), "{got}");
    assert!(
        got.get("form").is_none(),
        "a GET has no form key, as JSON.stringify omits undefined: {got}"
    );

    let empty: Value = serde_json::from_str(&post_form(port, "").await).expect("MB_REQUEST");
    assert!(
        empty.get("form").is_none(),
        "an empty urlencoded body has no form key: {empty}"
    );

    let _ = manager.delete_imposter(port).await;
}
