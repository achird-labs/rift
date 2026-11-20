//! Issue #1756: Rhai, JavaScript and templates read the imposter's clock and seed, as the wasm
//! engine, flow-store TTLs and `_rift.fault` already do (#1754, #1755).
#![cfg(feature = "javascript")]

use rift_mock_core::imposter::{ImposterConfig, ImposterManager};
use serde_json::{Value, json};

const FROZEN_MS: i64 = 1_700_000_000_000;

fn imposter(stubs: Vec<Value>, seed: Option<u64>) -> ImposterConfig {
    let mut engine = json!({});
    if let Some(seed) = seed {
        engine["seed"] = json!(seed);
    }
    serde_json::from_value(json!({
        "protocol": "http",
        "_rift": { "scriptEngine": engine },
        "stubs": stubs,
    }))
    .expect("config")
}

fn at(path: &str, response: Value) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }], "responses": [response] })
}

fn script(engine: &str, code: &str) -> Value {
    json!({ "_rift": { "script": { "engine": engine, "code": code } } })
}

async fn body(port: u16, path: &str) -> String {
    let resp = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200, "{path}");
    resp.text().await.expect("body")
}

fn time_stubs() -> Vec<Value> {
    vec![
        at(
            "/rhai",
            script("rhai", "fn respond(ctx) { http(200, timestamp_header()) }"),
        ),
        at(
            "/js",
            script(
                "javascript",
                "function respond(ctx) { return http(200, String(Date.now())); }",
            ),
        ),
        at(
            "/inject",
            json!({ "inject": "function (config) { return { statusCode: 200, body: String(new Date().getTime()) }; }" }),
        ),
        at(
            "/decorate",
            json!({ "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "decorate": "function (config, response) { response.body = String(Date.now()); }" } }),
        ),
        // Waits 0 ms when it reads the frozen clock, 2 s otherwise.
        at(
            "/wait",
            json!({ "is": { "statusCode": 200, "body": "waited" },
                    "_behaviors": { "wait": "function () { return Date.now() === 1700000000000 ? 0 : 2000; }" } }),
        ),
        // Answers later, from a timer: the run resumes, perhaps after another imposter's run.
        at(
            "/async",
            json!({ "inject": "function (config) { setTimeout(function () { config.callback({ statusCode: 200, body: String(Date.now()) }); }, 20); }" }),
        ),
        at(
            "/template",
            json!({ "is": { "statusCode": 200, "body": "{{ now }}" }, "_rift": { "templated": true } }),
        ),
    ]
}

#[tokio::test]
async fn every_engine_reads_the_frozen_imposter_clock() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(imposter(time_stubs(), None))
        .await
        .expect("create");
    manager
        .get_imposter(port)
        .expect("imposter")
        .clock()
        .freeze(FROZEN_MS)
        .expect("freeze");
    // 2023-11-14T22:13:20Z.
    assert_eq!(body(port, "/rhai").await, "Tue, 14 Nov 2023 22:13:20 GMT");
    assert_eq!(body(port, "/js").await, FROZEN_MS.to_string());
    assert_eq!(body(port, "/inject").await, FROZEN_MS.to_string());
    assert_eq!(body(port, "/decorate").await, FROZEN_MS.to_string());
    let started = std::time::Instant::now();
    assert_eq!(body(port, "/wait").await, "waited");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "the wait function read the frozen clock"
    );
    assert!(
        body(port, "/template")
            .await
            .starts_with("2023-11-14T22:13:20"),
        "template now"
    );
    // Back on wall time after a reset.
    manager
        .get_imposter(port)
        .expect("imposter")
        .clock()
        .reset();
    let wall: i64 = body(port, "/js").await.parse().expect("ms");
    assert!(wall > FROZEN_MS + 86_400_000, "{wall}");
    manager.delete_imposter(port).await.expect("delete");
}

// MB realms are shared across imposters on one worker: each run reads its own imposter's clock.
#[tokio::test]
async fn imposters_sharing_a_realm_each_read_their_own_clock() {
    let manager = ImposterManager::new();
    let mut ports = Vec::new();
    for offset in 0..4 {
        let port = manager
            .create_imposter(imposter(time_stubs(), None))
            .await
            .expect("create");
        manager
            .get_imposter(port)
            .expect("imposter")
            .clock()
            .freeze(FROZEN_MS + offset * 1_000)
            .expect("freeze");
        ports.push((port, FROZEN_MS + offset * 1_000));
    }
    for _ in 0..3 {
        for (port, frozen) in &ports {
            assert_eq!(body(*port, "/inject").await, frozen.to_string());
            assert_eq!(body(*port, "/js").await, frozen.to_string());
        }
    }
    // Concurrent runs that resume from timers, interleaved on the shared realms.
    let mut runs = tokio::task::JoinSet::new();
    for (port, frozen) in ports.clone() {
        runs.spawn(async move { (body(port, "/async").await, frozen) });
    }
    while let Some(joined) = runs.join_next().await {
        let (answer, frozen) = joined.expect("join");
        assert_eq!(answer, frozen.to_string());
    }
    for (port, _) in ports {
        manager.delete_imposter(port).await.expect("delete");
    }
}

fn random_stubs() -> Vec<Value> {
    vec![
        at(
            "/js",
            script(
                "javascript",
                "function respond(ctx) { return http(200, String(Math.random())); }",
            ),
        ),
        at(
            "/inject",
            json!({ "inject": "function (config) { return { statusCode: 200, body: String(Math.random()) }; }" }),
        ),
    ]
}

async fn draws(manager: &ImposterManager, seed: Option<u64>) -> Vec<String> {
    let port = manager
        .create_imposter(imposter(random_stubs(), seed))
        .await
        .expect("create");
    let mut out = Vec::new();
    for path in ["/js", "/inject", "/js", "/inject"] {
        out.push(body(port, path).await);
    }
    manager.delete_imposter(port).await.expect("delete");
    out
}

#[tokio::test]
async fn math_random_replays_under_a_seed() {
    let manager = ImposterManager::new();
    let first = draws(&manager, Some(42)).await;
    for value in &first {
        let n: f64 = value.parse().expect("a number");
        assert!((0.0..1.0).contains(&n), "{n}");
    }
    assert_eq!(
        draws(&manager, Some(42)).await,
        first,
        "the same seed, recreated"
    );
    assert_ne!(draws(&manager, Some(43)).await, first, "another seed");
    assert_ne!(
        draws(&manager, None).await,
        draws(&manager, None).await,
        "unseeded"
    );
}

// The legacy `{{NOW}}` token and the `now` function read the same clock.
#[tokio::test]
async fn the_legacy_now_token_reads_the_frozen_clock() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(imposter(
            vec![at(
                "/legacy",
                json!({ "is": { "statusCode": 200, "body": "{{NOW}}" } }),
            )],
            None,
        ))
        .await
        .expect("create");
    manager
        .get_imposter(port)
        .expect("imposter")
        .clock()
        .freeze(FROZEN_MS)
        .expect("freeze");
    assert!(
        body(port, "/legacy")
            .await
            .starts_with("2023-11-14T22:13:20")
    );
    manager.delete_imposter(port).await.expect("delete");
}

// A request's own X-Rift-Now reaches a Mountebank inject on the script pool.
#[tokio::test]
async fn a_request_instant_reaches_pool_javascript() {
    let manager = ImposterManager::new();
    let config: ImposterConfig = serde_json::from_value(json!({
        "protocol": "http",
        "_rift": { "scriptEngine": { "clock": "header" } },
        "stubs": time_stubs(),
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let client = reqwest::Client::new();
    for (path, instant) in [("/inject", "1234"), ("/js", "5678"), ("/inject", "42")] {
        let got = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .header("x-rift-now", instant)
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("body");
        assert_eq!(got, instant, "{path}");
    }
    manager.delete_imposter(port).await.expect("delete");
}

// A recording proxy's predicateGenerators inject reads the imposter's clock too.
#[tokio::test]
async fn a_predicate_generator_inject_reads_the_frozen_clock() {
    let manager = ImposterManager::new();
    let upstream = manager
        .create_imposter(imposter(
            vec![json!({ "responses": [{ "is": { "statusCode": 200, "body": "up" } }] })],
            None,
        ))
        .await
        .expect("upstream");
    let proxy = manager
        .create_imposter(imposter(
            vec![json!({ "responses": [{ "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"),
                "mode": "proxyOnce",
                "predicateGenerators": [{ "inject":
                    "function (config) { return [{ equals: { path: config.request.path, query: { at: String(Date.now()) } } }]; }" }]
            } }] })],
            None,
        ))
        .await
        .expect("proxy");
    manager
        .get_imposter(proxy)
        .expect("imposter")
        .clock()
        .freeze(FROZEN_MS)
        .expect("freeze");
    assert_eq!(body(proxy, "/rec").await, "up");
    let stubs = serde_json::to_value(manager.get_imposter(proxy).expect("imposter").get_stubs())
        .expect("stubs");
    assert!(
        stubs.to_string().contains(&FROZEN_MS.to_string()),
        "the generated predicate read the frozen clock: {stubs}"
    );
    manager.delete_imposter(proxy).await.expect("delete");
    manager.delete_imposter(upstream).await.expect("delete");
}
