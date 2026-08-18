//! Issue #969: end-to-end coverage for `_rift.stateOps` — declarative post-response flow-state
//! writes (`set`/`increment`/`delete`/`clearFlow`) on an `is` response, driven through the real
//! imposter serve loop (`ImposterManager` + reqwest), the same pattern as
//! `journal_match_outcome.rs`.
//!
//! Every imposter here uses `flowIdSource: "header:X-Session"` and a distinct session value per
//! test, so flow state never leaks across tests sharing the process, and the flow id to read back
//! is always known without reverse-engineering `resolve_flow_id`'s port-based default.
//!
//! What is NOT covered here: the `RIFT_DEBUG` 500 door (`x-rift-state-ops-error`). Like the
//! `_rift.templated` door before it (issue #687), it fires only under `RIFT_DEBUG` — and
//! `crate::util::rift_debug_env` caches that read in a `OnceLock` for the life of the process, so
//! no later test in this binary can toggle it even serially. Its shape is pinned instead by a unit
//! test, `imposter::handler::state_ops_error_tests`.

use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    // Give the listener a moment to bind before the request (matches the sibling HTTP tests).
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn get(port: u16, path: &str, session: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("X-Session", session)
        .send()
        .await
        .expect("send")
}

/// Read one flow-state key straight from the imposter's store, bypassing HTTP.
fn state(manager: &ImposterManager, port: u16, flow: &str, key: &str) -> Option<Value> {
    manager
        .get_imposter(port)
        .expect("imposter exists")
        .flow_store
        .get(flow, key)
        .expect("store read")
}

/// A one-stub imposter with `flowIdSource: header:X-Session` and the given `_rift` block on its
/// single `is` response.
fn one_stub_config(rift: Value, is: Value) -> Value {
    json!({
        "port": 0, "protocol": "http",
        "_rift": { "flowState": { "flowIdSource": "header:X-Session" } },
        "stubs": [{ "predicates": [], "responses": [{ "is": is, "_rift": rift }] }]
    })
}

// (a) `increment` counts across requests, accumulating on the resolved flow id.
#[tokio::test]
async fn increment_counts_across_requests() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [{ "op": "increment", "key": "hits" }] }),
            json!({ "statusCode": 200 }),
        ),
    )
    .await;

    for _ in 0..3 {
        assert_eq!(get(port, "/x", "s-incr").await.status(), 200);
    }

    assert_eq!(state(&manager, port, "s-incr", "hits"), Some(json!(3)));
    manager.delete_all().await;
}

// (b) `set` renders its value against the request (query params here).
#[tokio::test]
async fn set_renders_from_request_data() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [
                { "op": "set", "key": "lastId", "value": "{{ request.query.id }}" }
            ] }),
            json!({ "statusCode": 200 }),
        ),
    )
    .await;

    get(port, "/x?id=42", "s-set").await;

    assert_eq!(state(&manager, port, "s-set", "lastId"), Some(json!("42")));
    manager.delete_all().await;
}

// (c) `previousValue`: a `set` whose value mentions it is an accumulator across requests — empty
// on the first write, then whatever the prior write left.
#[tokio::test]
async fn previous_value_accumulates_across_requests() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [
                { "op": "set", "key": "trail", "value": "{{ previousValue }}|x" }
            ] }),
            json!({ "statusCode": 200 }),
        ),
    )
    .await;

    get(port, "/x", "s-prev").await;
    assert_eq!(state(&manager, port, "s-prev", "trail"), Some(json!("|x")));

    get(port, "/x", "s-prev").await;
    assert_eq!(
        state(&manager, port, "s-prev", "trail"),
        Some(json!("|x|x"))
    );

    manager.delete_all().await;
}

// (d) `delete` removes one key, leaving others; `clearFlow` removes every key in the flow.
#[tokio::test]
async fn delete_and_clear_flow() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http",
            "_rift": { "flowState": { "flowIdSource": "header:X-Session" } },
            "stubs": [
                { "predicates": [{ "equals": { "path": "/seed" } }],
                  "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "stateOps": [
                        { "op": "set", "key": "a", "value": "1" },
                        { "op": "set", "key": "b", "value": "2" }
                    ] } }] },
                { "predicates": [{ "equals": { "path": "/del" } }],
                  "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "stateOps": [{ "op": "delete", "key": "a" }] } }] },
                { "predicates": [{ "equals": { "path": "/clear" } }],
                  "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "stateOps": [{ "op": "clearFlow" }] } }] }
            ]
        }),
    )
    .await;

    get(port, "/seed", "s-del").await;
    assert_eq!(state(&manager, port, "s-del", "a"), Some(json!("1")));
    assert_eq!(state(&manager, port, "s-del", "b"), Some(json!("2")));

    get(port, "/del", "s-del").await;
    assert_eq!(state(&manager, port, "s-del", "a"), None, "a was deleted");
    assert_eq!(
        state(&manager, port, "s-del", "b"),
        Some(json!("2")),
        "b is untouched by deleting a"
    );

    get(port, "/clear", "s-del").await;
    assert_eq!(state(&manager, port, "s-del", "a"), None);
    assert_eq!(
        state(&manager, port, "s-del", "b"),
        None,
        "clearFlow took b too"
    );

    manager.delete_all().await;
}

// (e) A body reading `{{ state.hits }}` in the SAME response as an `increment` sees the value from
// BEFORE this request's own op — templating (which renders the body) runs first, `stateOps` runs
// last, right before the response is written.
#[tokio::test]
async fn body_in_the_same_response_reads_the_pre_op_value() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({
                "templated": true,
                "stateOps": [{ "op": "increment", "key": "hits" }]
            }),
            json!({ "statusCode": 200, "body": "{{ state.hits }}" }),
        ),
    )
    .await;

    let first = get(port, "/x", "s-order").await.text().await.expect("body");
    assert_eq!(
        first, "",
        "hits is absent before the first request's own increment runs"
    );

    let second = get(port, "/x", "s-order").await.text().await.expect("body");
    assert_eq!(
        second, "1",
        "the second request's body reads what the FIRST request's op wrote, not its own"
    );

    assert_eq!(state(&manager, port, "s-order", "hits"), Some(json!(2)));
    manager.delete_all().await;
}

// (f) Concurrency, no lost updates: `increment` is atomic on the in-memory backend, and a
// `previousValue` `set` is a bounded CAS loop — 50 concurrent requests must land exactly 50
// increments / 50 appended segments, never fewer.
// Multi-threaded on purpose: on the default current-thread test runtime the 50 requests would
// be serialized by the runtime itself and prove nothing about the atomicity claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_increments_lose_none() {
    const N: usize = 50;
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [{ "op": "increment", "key": "hits" }] }),
            json!({ "statusCode": 200 }),
        ),
    )
    .await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..N {
        tasks.spawn(async move {
            get(port, "/x", "s-conc-incr").await;
        });
    }
    while tasks.join_next().await.is_some() {}

    assert_eq!(
        state(&manager, port, "s-conc-incr", "hits"),
        Some(json!(N as i64)),
        "every concurrent increment must be counted"
    );
    manager.delete_all().await;
}

// Multi-threaded for the same reason as the increment test above; this is the compare-and-set
// loop under a real thundering herd on one key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_previous_value_appends_lose_none() {
    const N: usize = 50;
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [
                { "op": "set", "key": "trail", "value": "{{ previousValue }}x" }
            ] }),
            json!({ "statusCode": 200 }),
        ),
    )
    .await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..N {
        tasks.spawn(async move {
            get(port, "/x", "s-conc-cas").await;
        });
    }
    while tasks.join_next().await.is_some() {}

    let trail = state(&manager, port, "s-conc-cas", "trail").expect("trail was written");
    let trail = trail.as_str().expect("trail is a string").to_string();
    assert_eq!(
        trail.len(),
        N,
        "every concurrent CAS append must land — no lost update, got: {trail:?}"
    );
    manager.delete_all().await;
}

// (g) Absent `stateOps` (and an explicitly empty array) must serve byte-identically to a control
// response with no `_rift` block at all — status, headers (`date` excluded — it's wall-clock) and
// body all match.
#[tokio::test]
async fn stub_without_state_ops_serves_unchanged() {
    let manager = ImposterManager::new();
    let is = json!({
        "statusCode": 201,
        "headers": { "X-Custom": "yes" },
        "body": "hello"
    });
    let stub = |rift: Option<Value>| {
        let mut response = json!({ "is": is.clone() });
        if let Some(rift) = rift {
            response["_rift"] = rift;
        }
        json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "predicates": [], "responses": [response] }]
        })
    };

    let control_port = create(&manager, stub(None)).await;
    let empty_ops_port = create(&manager, stub(Some(json!({ "stateOps": [] })))).await;

    let control = get(control_port, "/x", "ignored").await;
    let control_status = control.status();
    let mut control_headers: Vec<(String, String)> = control
        .headers()
        .iter()
        .filter(|(k, _)| *k != "date")
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    control_headers.sort();
    let control_body = control.text().await.expect("body");

    let with_ops = get(empty_ops_port, "/x", "ignored").await;
    let with_ops_status = with_ops.status();
    let mut with_ops_headers: Vec<(String, String)> = with_ops
        .headers()
        .iter()
        .filter(|(k, _)| *k != "date")
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    with_ops_headers.sort();
    let with_ops_body = with_ops.text().await.expect("body");

    assert_eq!(control_status, with_ops_status);
    assert_eq!(control_headers, with_ops_headers);
    assert_eq!(control_body, with_ops_body);

    manager.delete_all().await;
}

// (h) `stateOps` round-trips through the imposter's own serialized config — the shape a
// `GET /imposters/:port` admin read would return (the config is serialized on that read path via
// this same `RiftResponseExtension`).
#[tokio::test]
async fn state_ops_round_trips_through_serialized_config() {
    let manager = ImposterManager::new();
    let ops = json!([
        { "op": "set", "key": "k", "value": "v" },
        { "op": "increment", "key": "hits", "by": 2 },
        { "op": "delete", "key": "tmp" },
        { "op": "clearFlow" }
    ]);
    let port = create(
        &manager,
        one_stub_config(json!({ "stateOps": ops }), json!({ "statusCode": 200 })),
    )
    .await;

    let imposter = manager.get_imposter(port).expect("imposter exists");
    let config_json = serde_json::to_value(&imposter.config).expect("config serializes");
    assert_eq!(
        config_json["stubs"][0]["responses"][0]["_rift"]["stateOps"], ops,
        "stateOps must round-trip byte-for-byte through the admin-read serialization: {config_json}"
    );

    manager.delete_all().await;
}

// (i) Non-debug leniency: a `set` whose value template fails to resolve (`request.query.missing`
// here) does not fail the request — the token substitutes empty (template_fn's own non-debug
// policy) and the stub's own response is still served. The debug-mode 500 door is pinned
// separately by a unit test (see the module doc for why it cannot run here).
#[tokio::test]
async fn non_debug_a_failing_set_template_still_serves_the_stub_response() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        one_stub_config(
            json!({ "stateOps": [
                { "op": "set", "key": "a", "value": "{{ request.query.missing }}" }
            ] }),
            json!({ "statusCode": 200, "body": "still served" }),
        ),
    )
    .await;

    let resp = get(port, "/x", "s-lenient").await;
    assert_eq!(
        resp.status(),
        200,
        "a non-debug template failure inside a stateOps set does not fail the request"
    );
    assert!(
        !resp.headers().contains_key("x-rift-state-ops-error"),
        "the debug-only error marker must be absent outside debug mode"
    );
    let body = resp.text().await.expect("body");
    assert_eq!(body, "still served");
    assert_eq!(
        state(&manager, port, "s-lenient", "a"),
        Some(json!("")),
        "the failing token substitutes empty rather than aborting the op"
    );

    manager.delete_all().await;
}
