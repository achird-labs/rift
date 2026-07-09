//! Issue #440: a Mountebank predicate `inject` function that THROWS must FAIL LOUD — a
//! Mountebank-shaped 400 error (`{"errors":[{"code":"invalid predicate injection", ...}]}`), not
//! a silent fall-through to "didn't match" (which would let a later stub, or the imposter's
//! default response, wrongly serve the request). Mountebank's own `InjectionError` fails the
//! request on this rather than treating the predicate as non-matching — mirrors the
//! response-inject error parity added for issue #355 Item 5
//! (`issue_355_inject_error_parity.rs`).

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

// A throwing predicate inject returns HTTP 400 with the Mountebank error body shape, and never
// falls through to a later stub — proven here by a catch-all fallback stub that must NOT be
// reached.
#[tokio::test]
async fn throwing_predicate_inject_returns_mountebank_400_not_fallthrough() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 19898, "protocol": "http", "stubs": [
            {
                "predicates": [
                    { "inject": "function (config) { throw new Error('boom-predicate'); }" }
                ],
                "responses": [{ "is": { "statusCode": 200, "body": "should never be served" } }]
            },
            {
                // Catch-all fallback: if the throwing predicate silently fell through to "no
                // match" (the pre-#440 bug), the request would land here and return 201 — the
                // fix requires this stub to NEVER be reached.
                "responses": [
                    { "is": { "statusCode": 201, "body": "fallback - must not be served either" } }
                ]
            }
        ]
    }))
    .expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:19898/x")
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        400,
        "a throwing predicate inject must fail the request with 400 (Mountebank error parity), \
         not fall through to another stub or a bare 500"
    );
    assert!(
        resp.headers().contains_key("x-rift-imposter"),
        "the imposter marker header must be preserved"
    );
    assert!(
        resp.headers().contains_key("x-rift-inject-error"),
        "the inject-error marker header must be present"
    );

    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["errors"][0]["code"], "invalid predicate injection",
        "error code must be 'invalid predicate injection', got: {body}"
    );
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("boom-predicate")),
        "the error message must surface the script failure, got: {body}"
    );

    let _ = manager.delete_imposter(19898).await;
}

// Happy-path parity check (no behavior change): a predicate inject that does NOT throw still
// matches / doesn't-match correctly, and a non-match falls through to the next stub as before.
#[tokio::test]
async fn non_erroring_predicate_inject_still_matches_correctly() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(serde_json::json!({
        "port": 19899, "protocol": "http", "stubs": [
            {
                "predicates": [
                    { "inject": "function (config) { return config.path === '/match'; }" }
                ],
                "responses": [{ "is": { "statusCode": 200, "body": "matched" } }]
            },
            {
                "responses": [{ "is": { "statusCode": 201, "body": "fallback" } }]
            }
        ]
    }))
    .expect("config");
    manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = reqwest::Client::new();

    let matched = client
        .get("http://127.0.0.1:19899/match")
        .send()
        .await
        .expect("send");
    assert_eq!(
        matched.status(),
        200,
        "a matching (Ok(true)) predicate inject must still match"
    );

    let not_matched = client
        .get("http://127.0.0.1:19899/other")
        .send()
        .await
        .expect("send");
    assert_eq!(
        not_matched.status(),
        201,
        "a non-matching (Ok(false)) predicate inject must still fall through to the next stub"
    );

    let _ = manager.delete_imposter(19899).await;
}
