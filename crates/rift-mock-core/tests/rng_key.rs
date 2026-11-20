//! Issue #1795: with `_rift.scriptEngine.rngKey`, each request draws from a stream picked by its
//! key, so concurrent requests get the same values whatever order they arrive in.

#[cfg(feature = "wasm")]
#[path = "../test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const KEY_HEADER: &str = "x-rift-rng-key";

fn at(path: &str, response: Value) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }], "responses": [response] })
}

fn config(engine: Value) -> Value {
    #[allow(unused_mut)]
    let mut stubs: Vec<Value> = vec![
        at(
            "/tpl",
            json!({ "is": { "statusCode": 200, "body": "{{ randomInt 1 1000000000 }}" },
                    "_rift": { "templated": true } }),
        ),
        at(
            "/fault",
            json!({ "is": { "statusCode": 200, "body": "ok" },
                    "_rift": { "fault": { "error": { "probability": 0.5, "status": 503, "body": "boom" } } } }),
        ),
    ];
    #[cfg(feature = "javascript")]
    stubs.push(at(
        "/js",
        json!({ "_rift": { "script": { "engine": "javascript",
            "code": "function respond(ctx) { return http(200, String(Math.random())); }" } } }),
    ));
    #[cfg(feature = "wasm")]
    stubs.push(at(
        "/wasm",
        json!({ "_rift": { "script": { "engine": "wasm",
            "code": wasm_fixtures::b64(&wasm_fixtures::random_body()) } } }),
    ));
    json!({ "protocol": "http", "_rift": { "scriptEngine": engine }, "stubs": stubs })
}

fn keyed() -> Value {
    json!({ "seed": 42, "rngKey": [format!("header:{KEY_HEADER}"), "path"] })
}

fn imposter(engine: Value) -> ImposterConfig {
    serde_json::from_value(config(engine)).expect("config")
}

fn paths() -> Vec<&'static str> {
    let mut paths: Vec<&'static str> = Vec::new();
    paths.extend(["/tpl", "/fault"]);
    #[cfg(feature = "javascript")]
    paths.push("/js");
    #[cfg(feature = "wasm")]
    paths.push("/wasm");
    paths
}

async fn send(client: &reqwest::Client, port: u16, path: &str, key: Option<&str>) -> String {
    let mut req = client.get(format!("http://127.0.0.1:{port}{path}"));
    if let Some(key) = key {
        req = req.header(KEY_HEADER, key);
    }
    let resp = req.send().await.expect("request");
    format!(
        "{} {}",
        resp.status().as_u16(),
        resp.text().await.expect("body")
    )
}

/// Every (key, path) answered, sent `concurrency` at a time in `order`.
async fn answers(
    manager: &ImposterManager,
    order: &[(String, &'static str)],
    concurrency: usize,
) -> BTreeMap<(String, &'static str), String> {
    let port = manager
        .create_imposter(imposter(keyed()))
        .await
        .expect("create");
    let client = reqwest::Client::new();
    let mut out = BTreeMap::new();
    for chunk in order.chunks(concurrency) {
        let mut set = tokio::task::JoinSet::new();
        for (key, path) in chunk.iter().cloned() {
            let client = client.clone();
            set.spawn(async move {
                let answer = send(&client, port, path, Some(&key)).await;
                ((key, path), answer)
            });
        }
        while let Some(joined) = set.join_next().await {
            let (k, v) = joined.expect("join");
            out.insert(k, v);
        }
    }
    manager.delete_imposter(port).await.expect("delete");
    out
}

// The issue's main case: 200 keys, 32 at a time in a shuffled order, answer as the same keys sent
// one by one.
#[tokio::test]
async fn concurrent_keyed_requests_draw_as_serial_ones_do() {
    let manager = ImposterManager::new();
    let mut order: Vec<(String, &'static str)> = (0..200)
        .flat_map(|i| paths().into_iter().map(move |p| (format!("k{i}"), p)))
        .collect();
    let serial = answers(&manager, &order, 1).await;
    // A fixed shuffle: reverse within each path, then interleave the paths differently.
    order.sort_by_key(|(key, path)| (std::cmp::Reverse(key.clone()), *path));
    let third = order.len() / 3;
    order.rotate_left(third);
    let concurrent = answers(&manager, &order, 32).await;
    assert_eq!(serial, concurrent);
    // The keys draw different values: not one stream for all.
    let templated: std::collections::BTreeSet<&String> = serial
        .iter()
        .filter(|((_, path), _)| *path == "/tpl")
        .map(|(_, v)| v)
        .collect();
    assert!(templated.len() > 190, "{} distinct of 200", templated.len());
    let faults = serial
        .iter()
        .filter(|((_, path), v)| *path == "/fault" && v.starts_with("503"))
        .count();
    assert!((60..140).contains(&faults), "{faults} of 200 faulted");
}

// The same key twice draws a fresh stream each time (its occurrence), the same on every run, and
// a restart or a recreate starts the count again.
#[tokio::test]
async fn a_repeated_key_draws_fresh_values_and_restart_resets_the_count() {
    let manager = ImposterManager::new();
    let client = reqwest::Client::new();
    let run = |port: u16| {
        let client = client.clone();
        async move {
            (
                send(&client, port, "/tpl", Some("same")).await,
                send(&client, port, "/tpl", Some("same")).await,
            )
        }
    };
    let port = manager
        .create_imposter(imposter(keyed()))
        .await
        .expect("create");
    let (a, b) = run(port).await;
    assert_ne!(a, b, "a second request with one key draws another value");
    manager.restart_runtime_state(port).await.expect("restart");
    assert_eq!(run(port).await, (a.clone(), b.clone()), "after a restart");
    manager.delete_imposter(port).await.expect("delete");
    let port = manager
        .create_imposter(imposter(keyed()))
        .await
        .expect("create");
    assert_eq!(run(port).await, (a, b), "after a recreate");
    manager.delete_imposter(port).await.expect("delete");
}

// A request whose key parts are all empty (here: the header missing) draws from the imposter's
// stream, exactly as it would without rngKey.
#[tokio::test]
async fn a_request_without_its_key_uses_the_imposter_stream() {
    let manager = ImposterManager::new();
    let client = reqwest::Client::new();
    let header_only = json!({ "seed": 42, "rngKey": [format!("header:{KEY_HEADER}")] });
    let first_draw = |engine: Value| {
        let (manager, client) = (&manager, &client);
        async move {
            let port = manager
                .create_imposter(imposter(engine))
                .await
                .expect("create");
            let answer = send(client, port, "/tpl", None).await;
            manager.delete_imposter(port).await.expect("delete");
            answer
        }
    };
    assert_eq!(
        first_draw(header_only).await,
        first_draw(json!({ "seed": 42 })).await
    );
}

// Without rngKey nothing changes: the imposter's one stream, #1755's behaviour.
#[tokio::test]
async fn without_rng_key_requests_share_the_imposter_stream() {
    let manager = ImposterManager::new();
    let client = reqwest::Client::new();
    let mut runs = Vec::new();
    for _ in 0..2 {
        let port = manager
            .create_imposter(imposter(json!({ "seed": 42 })))
            .await
            .expect("create");
        let mut out = Vec::new();
        for key in ["a", "b", "a"] {
            out.push(send(&client, port, "/tpl", Some(key)).await);
        }
        manager.delete_imposter(port).await.expect("delete");
        runs.push(out);
    }
    assert_eq!(runs[0], runs[1], "seeded, serialized: replays");
    assert_ne!(
        runs[0][0], runs[0][2],
        "one stream: the key is not consulted"
    );
}

// Refused at admission, naming the problem; decoded from storage, accepted.
#[test]
fn admission_refuses_what_replay_accepts() {
    for (engine, why) in [
        (
            json!({ "seed": 1, "rngKey": ["cookie:x"] }),
            "is not one of",
        ),
        (json!({ "seed": 1, "rngKey": [] }), "at least one"),
        (
            json!({ "seed": 1, "rngKey": ["body:no-slash"] }),
            "RFC 6901",
        ),
        (
            json!({ "rngKey": ["path"] }),
            "needs _rift.scriptEngine.seed",
        ),
        (json!({ "seed": 1, "rngKey": "path" }), "list of strings"),
    ] {
        let config: ImposterConfig =
            rift_mock_core::imposter::deserialize_replayed(config(engine.clone()))
                .expect("a stored config replays");
        let err = rift_mock_core::imposter::admission_check(&config).expect_err("refused");
        assert!(err.contains(why), "{engine}: {err}");
    }
    let config: ImposterConfig =
        rift_mock_core::imposter::deserialize_replayed(config(keyed())).expect("decodes");
    assert!(rift_mock_core::imposter::admission_check(&config).is_ok());
}
