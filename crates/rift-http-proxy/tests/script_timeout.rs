//! Issue #308: a runaway `_rift.script` (`respond(ctx)`) is bounded by
//! `_rift.scriptEngine.timeoutMs` and does NOT wedge unrelated imposters.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cfg(v: serde_json::Value) -> ImposterConfig {
    serde_json::from_value(v).expect("test imposter config")
}

// A runaway respond(ctx) on one imposter must time out AND leave a sibling imposter
// responsive — the exact end-to-end symptom from the bug report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runaway_script_times_out_and_sibling_stays_responsive() {
    let manager = Arc::new(ImposterManager::new());

    // Imposter A: an infinite-loop Rhai script (bare expression, issue #357 Item 2 — the whole
    // body IS the entrypoint, so the loop actually runs), bounded to 500ms.
    manager
        .create_imposter(cfg(serde_json::json!({
            "protocol": "http", "port": 19191,
            "_rift": { "scriptEngine": { "defaultEngine": "rhai", "timeoutMs": 500 } },
            "stubs": [{
                "predicates": [{ "equals": { "path": "/hang" } }],
                "responses": [{ "_rift": { "script": { "engine": "rhai",
                    "code": "let i = 0; loop { i += 1; }" } } }]
            }]
        })))
        .await
        .expect("create A");

    // Imposter B: a plain, unrelated imposter.
    manager
        .create_imposter(cfg(serde_json::json!({
            "protocol": "http", "port": 19192,
            "stubs": [{
                "predicates": [{ "equals": { "path": "/health" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "ok" } }]
            }]
        })))
        .await
        .expect("create B");

    // Sibling responds before the runaway even starts.
    let before = reqwest::get("http://127.0.0.1:19192/health")
        .await
        .expect("B reachable")
        .status();
    assert_eq!(before, 200);

    // Fire the runaway request in the background (do not await it fully).
    let hang = tokio::spawn(async {
        let start = Instant::now();
        let resp = reqwest::Client::new()
            .get("http://127.0.0.1:19191/hang")
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        (start.elapsed(), resp)
    });

    // While the runaway script is running, the unrelated imposter must stay responsive.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let sibling_start = Instant::now();
    let during = reqwest::Client::new()
        .get("http://127.0.0.1:19192/health")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .expect("sibling must respond while a script runs away")
        .status();
    assert_eq!(during, 200, "sibling imposter must not be wedged");
    assert!(
        sibling_start.elapsed() < Duration::from_secs(2),
        "sibling response was delayed by the runaway script: {:?}",
        sibling_start.elapsed()
    );

    // The runaway request itself is bounded: it returns a 500 near the 500ms timeout,
    // not after the client's 10s ceiling.
    let (elapsed, resp) = hang.await.expect("hang task");
    let resp = resp.expect("runaway request returns a response, not a hang");
    assert_eq!(resp.status(), 500, "runaway script yields a 500");
    assert!(
        elapsed < Duration::from_secs(5),
        "runaway must be interrupted near timeoutMs, took {elapsed:?}"
    );

    manager.delete_all().await;
}

// The non-runaway handler arms still work over HTTP after the #308 rewrite: a `None`
// decision → 200, and a `Latency` decision → 200 with the latency header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_script_none_and_latency_serve_200() {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(cfg(serde_json::json!({
            "protocol": "http", "port": 19193,
            "stubs": [
                { "predicates": [{ "equals": { "path": "/pass" } }],
                  "responses": [{ "_rift": { "script": { "engine": "rhai",
                      "code": "fn respond(ctx){ pass() }" } } }] },
                { "predicates": [{ "equals": { "path": "/slow" } }],
                  "responses": [{ "_rift": { "script": { "engine": "rhai",
                      "code": "fn respond(ctx){ delay(20) }" } } }] }
            ]
        })))
        .await
        .expect("create");

    let pass = reqwest::get("http://127.0.0.1:19193/pass")
        .await
        .expect("pass reachable");
    assert_eq!(pass.status(), 200, "None decision serves 200");

    let slow = reqwest::get("http://127.0.0.1:19193/slow")
        .await
        .expect("slow reachable");
    assert_eq!(slow.status(), 200, "Latency decision serves 200");
    assert!(
        slow.headers().contains_key("x-rift-latency-ms"),
        "latency response carries the latency header"
    );

    manager.delete_all().await;
}
