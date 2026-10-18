//! Issue #1553: a repeated query or form key reaches the predicate engine as several values, with
//! Mountebank's array rule (`predicates.js` `predicateSatisfied`, `deepEquals`).
//!
//! - A scalar expectation holds when **any** value satisfies it.
//! - An array expectation needs **every** element satisfied by some value, in any order, extra
//!   values allowed.
//! - `deepEquals` needs the same values as a multiset.
//! - A one-element array against a key sent once does not match, as in Mountebank.
//!
//! Each test drives a real imposter, so it observes what a client sees. The recorded request, verify
//! and `predicateGenerators` move with matching: a recording made from a repeated key must replay to
//! the request that produced it.

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

async fn imposter(manager: &ImposterManager, config: Value) -> u16 {
    let config = serde_json::from_value(config).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// An imposter with one stub that answers `hit` for `predicate`; anything else gets the default.
async fn with_predicate(manager: &ImposterManager, predicate: Value) -> u16 {
    imposter(
        manager,
        json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [predicate],
                "responses": [{ "is": { "statusCode": 200, "body": "hit" } }]
            }]
        }),
    )
    .await
}

async fn get(port: u16, path_and_query: &str, headers: &[(&str, &str)]) -> String {
    let mut request =
        reqwest::Client::new().get(format!("http://127.0.0.1:{port}{path_and_query}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body")
}

async fn hits(predicate: Value, path_and_query: &str) -> bool {
    let manager = ImposterManager::new();
    let port = with_predicate(&manager, predicate).await;
    let body = get(port, path_and_query, &[]).await;
    let _ = manager.delete_imposter(port).await;
    body == "hit"
}

#[tokio::test]
async fn a_scalar_expectation_holds_when_any_value_of_a_repeated_key_satisfies_it() {
    let q = "/p?key=first&key=second";
    assert!(hits(json!({"equals": {"query": {"key": "first"}}}), q).await);
    assert!(hits(json!({"equals": {"query": {"key": "second"}}}), q).await);
    assert!(hits(json!({"startsWith": {"query": {"key": "sec"}}}), q).await);
    assert!(hits(json!({"matches": {"query": {"key": "^first$"}}}), q).await);
    // `not` inverts the any-of answer: some value equals `first`, so the negation misses.
    assert!(!hits(json!({"not": {"equals": {"query": {"key": "first"}}}}), q).await);
}

#[tokio::test]
async fn an_array_expectation_needs_each_element_among_the_values_in_any_order() {
    assert!(
        hits(
            json!({"equals": {"query": {"key": ["first", "second"]}}}),
            "/p?key=second&key=first"
        )
        .await
    );
    // A subset: extra actual values are allowed (#1548's `?q=2&q=1&q=3`).
    assert!(
        hits(
            json!({"equals": {"query": {"q": ["1", "2"]}}}),
            "/p?q=2&q=1&q=3"
        )
        .await
    );
    assert!(
        hits(
            json!({"contains": {"query": {"key": ["fir", "sec"]}}}),
            "/p?key=first1&key=second2"
        )
        .await
    );
    // Case folds per element (the default is case-insensitive).
    assert!(
        hits(
            json!({"equals": {"query": {"key": ["FIRST", "SECOND"]}}}),
            "/p?key=first&key=second"
        )
        .await
    );
    // An element missing from the values fails the whole array.
    assert!(
        !hits(
            json!({"equals": {"query": {"key": ["first", "third"]}}}),
            "/p?key=first&key=second"
        )
        .await
    );
}

#[tokio::test]
async fn an_array_against_a_key_sent_once_does_not_match_as_in_mountebank() {
    assert!(
        !hits(
            json!({"equals": {"query": {"key": ["first"]}}}),
            "/p?key=first"
        )
        .await
    );
    assert!(
        hits(
            json!({"equals": {"query": {"key": ["first"]}}}),
            "/p?key=first&key=first"
        )
        .await
    );
}

#[tokio::test]
async fn deep_equals_compares_a_repeated_key_as_a_multiset() {
    let deep = |v: Value| json!({"deepEquals": {"query": {"key": v}}});
    assert!(hits(deep(json!(["second", "first"])), "/p?key=first&key=second").await);
    assert!(
        !hits(
            deep(json!(["first", "second"])),
            "/p?key=first&key=second&key=third"
        )
        .await
    );
    assert!(hits(deep(json!(["first", "first"])), "/p?key=first&key=first").await);
    assert!(!hits(deep(json!(["first", "first"])), "/p?key=first&key=second").await);
    assert!(!hits(deep(json!("first")), "/p?key=first&key=second").await);
}

#[tokio::test]
async fn the_old_comma_joined_form_matches_only_a_value_that_carries_the_comma() {
    let joined = json!({"equals": {"query": {"key": "first,second"}}});
    assert!(!hits(joined.clone(), "/p?key=first&key=second").await);
    assert!(hits(joined, "/p?key=first,second").await);
    // Nor is one comma-carrying value split into two at match time.
    assert!(
        !hits(
            json!({"equals": {"query": {"key": ["first", "second"]}}}),
            "/p?key=first,second"
        )
        .await
    );
}

#[tokio::test]
async fn a_repeated_form_key_is_matched_value_by_value() {
    let manager = ImposterManager::new();
    for (predicate, expected) in [
        (
            json!({"equals": {"form": {"key": ["first", "second"]}}}),
            true,
        ),
        (
            json!({"deepEquals": {"form": {"key": ["second", "first"]}}}),
            true,
        ),
        (json!({"equals": {"form": {"key": "first"}}}), true),
        (json!({"equals": {"form": {"key": "first,second"}}}), false),
    ] {
        let port = with_predicate(&manager, predicate.clone()).await;
        let body = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/f"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("key=first&key=second")
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body");
        let _ = manager.delete_imposter(port).await;
        assert_eq!(body == "hit", expected, "{predicate}");
    }
}

#[tokio::test]
async fn an_array_expectation_on_a_repeated_header_matches() {
    let manager = ImposterManager::new();
    let port = with_predicate(
        &manager,
        json!({"equals": {"headers": {"X-K": ["second", "first"]}}}),
    )
    .await;
    let body = get(port, "/", &[("X-K", "first"), ("X-K", "second")]).await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(body, "hit");
}

#[tokio::test]
async fn the_recorded_request_shows_a_repeated_key_as_an_array_and_verify_agrees() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        json!({"protocol": "http", "recordRequests": true, "stubs": []}),
    )
    .await;
    get(port, "/r?key=first&key=second&one=1", &[]).await;

    let imp = manager.get_imposter(port).expect("imposter");
    let recorded = serde_json::to_value(imp.get_recorded_requests()).expect("serialize");
    assert_eq!(
        recorded[0]["query"],
        json!({"key": ["first", "second"], "one": "1"})
    );

    let verify = |predicates: Value| {
        let opts = serde_json::from_value(json!({ "predicates": predicates })).expect("opts");
        imp.verify(&opts).expect("verify").matched
    };
    assert_eq!(
        verify(json!([{"equals": {"query": {"key": ["second", "first"]}}}])),
        1
    );
    assert_eq!(verify(json!([{"equals": {"query": {"key": "first"}}}])), 1);
    assert_eq!(
        verify(json!([{"equals": {"query": {"key": "first,second"}}}])),
        0
    );
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_generated_predicate_from_a_repeated_key_replays_in_any_order() {
    let manager = ImposterManager::new();
    let backend = imposter(
        &manager,
        json!({"protocol": "http", "stubs": [{"responses": [{"is": {"body": "up"}}]}]}),
    )
    .await;
    let proxy = imposter(
        &manager,
        json!({
            "protocol": "http",
            "stubs": [{"responses": [{"proxy": {
                "to": format!("http://127.0.0.1:{backend}"),
                "mode": "proxyOnce",
                "predicateGenerators": [{"matches": {"query": true}}]
            }}]}]
        }),
    )
    .await;
    assert_eq!(get(proxy, "/p?key=first&key=second", &[]).await, "up");

    let stubs = serde_json::to_value(manager.get_imposter(proxy).expect("proxy").get_stubs())
        .expect("stubs");
    let recorded: Vec<&Value> = stubs
        .as_array()
        .expect("stubs")
        .iter()
        .filter_map(|stub| stub["predicates"].as_array())
        .flatten()
        .collect();
    assert_eq!(
        recorded,
        [&json!({"deepEquals": {"query": {"key": ["first", "second"]}}})]
    );

    // With the backend gone, only the recorded stub can answer.
    let _ = manager.delete_imposter(backend).await;
    assert_eq!(get(proxy, "/p?key=second&key=first", &[]).await, "up");
    let _ = manager.delete_imposter(proxy).await;
}

#[tokio::test]
async fn copy_from_a_repeated_query_key_takes_its_first_value() {
    let manager = ImposterManager::new();
    let port = imposter(
        &manager,
        json!({
            "protocol": "http",
            "stubs": [{"responses": [{
                "is": {"statusCode": 200, "body": "${K}"},
                "_behaviors": {"copy": [{
                    "from": {"query": "key"},
                    "into": "${K}",
                    "using": {"method": "regex", "selector": ".*"}
                }]}
            }]}]
        }),
    )
    .await;
    let body = get(port, "/c?key=first&key=second", &[]).await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(body, "first");
}
