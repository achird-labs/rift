//! Issue #1754: an imposter's own clock. Its built-in flow store's TTLs follow it, so a test steps
//! past a TTL instead of sleeping through it; under `_rift.scriptEngine.clock: "header"` a request's
//! `X-Rift-Now` is its own instant; a restart puts it back on wall time.

use rift_mock_core::imposter::clock::ClockMode;
use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::{Value, json};

fn cfg(v: Value) -> ImposterConfig {
    serde_json::from_value(v).expect("imposter config")
}

/// `/set` writes `k` in the request's flow (a 60 s TTL); `/get` answers it, empty when absent.
fn ttl_imposter(clock: Option<&str>) -> Value {
    let mut engine = json!({});
    if let Some(source) = clock {
        engine["clock"] = json!(source);
    }
    json!({
        "protocol": "http",
        "_rift": { "flowState": { "ttlSeconds": 60 }, "scriptEngine": engine },
        "stubs": [
            { "predicates": [{ "equals": { "path": "/set" } }],
              "responses": [{ "is": { "statusCode": 200 },
                  "_rift": { "stateOps": [{ "op": "set", "key": "k", "value": "v" }] } }] },
            { "predicates": [{ "equals": { "path": "/get" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "[{{ state.k }}]" },
                  "_rift": { "templated": true } }] }
        ]
    })
}

async fn get(port: u16, path: &str, now: Option<&str>) -> (u16, String) {
    let mut req = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("x-flow-id", "f");
    if let Some(now) = now {
        req = req.header("x-rift-now", now);
    }
    let resp = req.send().await.expect("request");
    (resp.status().as_u16(), resp.text().await.expect("body"))
}

// The first TTL test that does not sleep: a step past the TTL expires the key.
#[tokio::test]
async fn a_ttl_follows_the_imposter_clock() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(cfg(ttl_imposter(None)))
        .await
        .expect("create");
    let imposter = manager.get_imposter(port).expect("imposter");
    imposter.clock().freeze(1_700_000_000_000).expect("freeze");
    assert_eq!(get(port, "/set", None).await.0, 200);
    assert_eq!(get(port, "/get", None).await, (200, "[v]".into()));
    imposter.clock().advance(59_000).expect("advance");
    assert_eq!(
        get(port, "/get", None).await,
        (200, "[v]".into()),
        "59 s: still there"
    );
    imposter.clock().advance(2_000).expect("advance");
    assert_eq!(
        get(port, "/get", None).await,
        (200, "[]".into()),
        "61 s: expired"
    );
}

// Under `clock: "header"`, each request reads at its own instant, concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_request_reads_at_its_own_x_rift_now() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(cfg(ttl_imposter(Some("header"))))
        .await
        .expect("create");
    assert_eq!(get(port, "/set", Some("1700000000000")).await.0, 200);
    let (early, late) = tokio::join!(
        get(port, "/get", Some("1700000030000")),
        get(port, "/get", Some("1700000061000")),
    );
    assert_eq!(early, (200, "[v]".into()), "30 s after the write");
    assert_eq!(late, (200, "[]".into()), "61 s after the write");
}

// A malformed `X-Rift-Now` is refused under `clock: "header"`, and ignored under `clock: "wall"`.
#[tokio::test]
async fn a_malformed_x_rift_now_is_refused_only_in_header_mode() {
    let manager = ImposterManager::new();
    let header = manager
        .create_imposter(cfg(ttl_imposter(Some("header"))))
        .await
        .expect("create");
    let (status, body) = get(header, "/get", Some("soon")).await;
    assert_eq!(status, 400);
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["errors"][0]["type"], "bad data");
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("X-Rift-Now")),
        "{body}"
    );

    let wall = manager
        .create_imposter(cfg(ttl_imposter(None)))
        .await
        .expect("create");
    assert_eq!(get(wall, "/get", Some("soon")).await.0, 200);
    // And a well-formed header changes nothing there: the write at wall time is still live.
    assert_eq!(get(wall, "/set", Some("0")).await.0, 200);
    assert_eq!(
        get(wall, "/get", Some("9999999999999")).await,
        (200, "[v]".into())
    );
}

// The clock is runtime state: a restart puts it back on wall time, and a recreated imposter starts
// on wall time.
#[tokio::test]
async fn a_restart_or_recreate_resets_the_clock() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(cfg(ttl_imposter(None)))
        .await
        .expect("create");
    let imposter = manager.get_imposter(port).expect("imposter");
    imposter.clock().freeze(5).expect("freeze");
    manager.restart_runtime_state(port).await.expect("restart");
    assert_eq!(imposter.clock().mode(), ClockMode::Wall);

    imposter.clock().freeze(5).expect("freeze");
    manager.delete_imposter(port).await.expect("delete");
    let mut config = ttl_imposter(None);
    config["port"] = json!(port);
    manager
        .create_imposter(cfg(config))
        .await
        .expect("recreate");
    let recreated = manager.get_imposter(port).expect("imposter");
    assert_eq!(recreated.clock().mode(), ClockMode::Wall);
}
