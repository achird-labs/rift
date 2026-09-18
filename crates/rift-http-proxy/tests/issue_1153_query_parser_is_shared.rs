//! Issue #1153: a request's query is parsed by one function for every surface that reads it.
//!
//! Templates used to parse with the orphaned `rift_mock_core::predicate` copy, which keeps the
//! *last* value of a repeated key, while predicates, the recorded request and scripts used the live
//! parser, which comma-joins every value in order. So one request could match a predicate on
//! `color = "red,green"` and then render `${request.query.color}` as `green` in the same response.
//!
//! This drives both surfaces on one real request so the agreement is observed, not assumed: the stub
//! only matches if the predicate sees `red,green`, and its body only echoes `red,green` if the
//! template sees it too.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

#[tokio::test]
async fn a_predicate_and_a_template_see_the_same_repeated_query_value() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 21930, "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "query": { "color": "red,green" } } }],
            "responses": [{ "is": {
                "statusCode": 200,
                "body": "${request.query.color}"
            } }]
        }]
    }))
    .expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:21930/p?color=red&color=green")
        .send()
        .await
        .expect("send");

    assert!(
        !resp.headers().contains_key("x-rift-no-match"),
        "the predicate must match: it sees the repeated key comma-joined"
    );
    assert_eq!(
        resp.text().await.expect("body"),
        "red,green",
        "the template must render the same value the predicate matched, not the last one"
    );
    let _ = manager.delete_imposter(21930).await;
}

// The `{{ }}` syntax (`_rift.templated`) reads the same `RequestData.query`, so the fix reaches it
// too — but "reaches it by construction" is an assumption until a test observes it. The triage named
// both template syntaxes; this pins the second one.
#[tokio::test]
async fn the_templated_syntax_also_renders_a_repeated_key_comma_joined() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 21931, "protocol": "http",
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "{{ request.query.color }}" },
                "_rift": { "templated": true }
            }]
        }]
    }))
    .expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:21931/p?color=red&color=green")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.text().await.expect("body"), "red,green");
    let _ = manager.delete_imposter(21931).await;
}

// The CHANGELOG promises the public path is unchanged after the parser moved to `util`. Only
// in-crate call sites guarded that re-export; this references it from outside the crate, through
// the path an embedder of `rift-http-proxy` would use, so dropping the re-export fails to compile.
#[test]
fn the_public_parse_query_string_path_still_resolves() {
    use rift_http_proxy::imposter::parse_query_string;
    let params = parse_query_string("color=red&color=green");
    assert_eq!(params.get("color").map(String::as_str), Some("red,green"));
}
