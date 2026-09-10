//! Issue #999 gate: every metric family `docs/features/metrics.md` documents must actually be
//! written by a real serve path.
//!
//! Nine of the thirteen documented families had no writer at all. Because the `lazy_static!`
//! families register on first *touch*, nothing touching them meant they were **absent** from the
//! scrape rather than zero — so `absent()` alerts fired and `rate()` queries returned nothing.
//!
//! The check that matters here is the anti-drift one: the family names are parsed out of the
//! published table and each is required to appear in `collect_metrics()` after real traffic. The
//! traffic must be what registers them — a test that recorded the metric itself first would pass
//! against the very defect it exists to catch.

use rift_http_proxy::imposter::ImposterManager;
use std::time::Duration;

/// The published table, embedded at compile time so a moved or renamed docs file breaks the build
/// loudly instead of silently skipping the comparison.
const METRICS_DOCS: &str = include_str!("../../../docs/features/metrics.md");

/// The heading whose table this guard governs. Scoping matters: `metrics.md` documents a **second**
/// endpoint further down — `## Admin GET /metrics (port 2525)` — whose two families
/// (`rift_imposters_total`, `rift_imposter_requests_total`) are a hand-written text body built in
/// `admin_api::handlers::system::handle_metrics`, not registry families. They are correctly
/// documented and correctly absent from `collect_metrics()`, so a whole-file parse here would
/// report them as unwritten and be wrong. Do not widen this to the whole file.
const REGISTRY_SECTION: &str = "## Prometheus endpoint metrics (port 9090)";

/// Metric family names from the first column of the `:9090` table — rows shaped
/// `| `rift_something` | counter | ... |` — stopping at the next `## ` heading.
fn documented_families() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut in_section = false;
    for line in METRICS_DOCS.lines() {
        if line.starts_with("## ") {
            in_section = line.trim_end() == REGISTRY_SECTION;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(rest) = line.trim_start().strip_prefix("| `rift_") else {
            continue;
        };
        let Some(end) = rest.find('`') else {
            continue;
        };
        let name = format!("rift_{}", &rest[..end]);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

async fn mk(manager: &ImposterManager, cfg: serde_json::Value) {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
}

/// Drive one request at `port`, ignoring the outcome — several of these deliberately provoke a
/// fault, an error status or a reset, and the metric is the only thing under test.
async fn hit(client: &reqwest::Client, port: u16, path: &str) {
    let _ = client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
}

#[tokio::test]
async fn documented_metric_families_all_appear_in_the_scrape() {
    let families = documented_families();
    // The `:9090` table has 12 rows (11 before `rift_preface_failures_total`, issue #1045). A
    // parser that silently dropped a few would leave exactly those families unchecked, which is
    // the failure this guard is supposed to make impossible — so the floor is the real count, not
    // a loose lower bound.
    assert_eq!(
        families.len(),
        12,
        "expected 11 documented `:9090` families; the table shape or contents changed. If a row \
         was deliberately added or removed, update this count deliberately too: {families:?}"
    );

    let manager = ImposterManager::new();

    // `_rift.fault` error at probability 1.0 -> faults_injected_total + error_status_total.
    mk(
        &manager,
        serde_json::json!({
            "port": 21500, "protocol": "http", "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "ok" },
                "_rift": { "fault": { "error": { "probability": 1.0, "status": 503, "body": "boom" } } }
            }] }]
        }),
    )
    .await;

    // `_rift.fault` latency -> latency_injected_ms. 1ms so the suite does not pay for the signal.
    mk(
        &manager,
        serde_json::json!({
            "port": 21501, "protocol": "http", "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "ok" },
                "_rift": { "fault": { "latency": { "probability": 1.0, "ms": 1 } } }
            }] }]
        }),
    )
    .await;

    // A script that decides an error fault -> script_execution_duration_ms
    // + faults_injected_total{source="script"}.
    mk(
        &manager,
        serde_json::json!({
            "port": 21502, "protocol": "http", "stubs": [{ "responses": [{
                "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { http(503, \"scripted\") }" } }
            }] }]
        }),
    )
    .await;

    // A script that throws -> script_errors_total.
    mk(
        &manager,
        serde_json::json!({
            "port": 21503, "protocol": "http", "stubs": [{ "responses": [{
                "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { throw \"deliberate\" }" } }
            }] }]
        }),
    )
    .await;

    // A script touching flow state -> flow_state_ops_total, through the metered store decorator.
    mk(
        &manager,
        serde_json::json!({
            "port": 21504, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory" } },
            "stubs": [{ "responses": [{
                "_rift": { "script": { "engine": "rhai",
                    "code": "fn respond(ctx) { let n = ctx.state.incr(\"c\"); http(200, `${n}`) }" } }
            }] }]
        }),
    )
    .await;

    // An in-test origin plus a proxy stub pointing at it -> upstream_request_duration_ms.
    mk(
        &manager,
        serde_json::json!({
            "port": 21505, "protocol": "http", "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "origin" } }] }]
        }),
    )
    .await;
    mk(
        &manager,
        serde_json::json!({
            "port": 21506, "protocol": "http", "stubs": [{ "responses": [{
                "proxy": { "to": "http://127.0.0.1:21505", "mode": "proxyAlways" } }] }]
        }),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = reqwest::Client::new();
    for port in [21500u16, 21501, 21502, 21503, 21504, 21506] {
        hit(&client, port, "/x").await;
    }

    let scrape = rift_http_proxy::extensions::collect_metrics();
    let missing: Vec<&String> = families
        .iter()
        .filter(|f| !scrape.contains(f.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "these families are documented in docs/features/metrics.md but absent from the scrape \
         after real traffic: {missing:?}\n\
         A documented family with no writer is invisible to Prometheus entirely (not zero), so \
         `absent()` alerts fire and `rate()` returns nothing. Either wire it, or delete its row.",
    );

    // Presence alone cannot tell a correctly-labelled series from a mislabelled one — and a
    // family written twice under two different `source` values looks identical to one written
    // once. These pin the label contract the docs state: `rule_id` is the imposter port, and
    // `source` is `rift` for a `_rift.fault` decision or `script` for a script's.
    // NB: the Prometheus text encoder emits labels in ALPHABETICAL order, not the order they are
    // declared in the `CounterVec`. Write them sorted or these never match.
    for expected in [
        r#"rift_faults_injected_total{rule_id="21500",source="rift",type="error"}"#,
        r#"rift_faults_injected_total{rule_id="21501",source="rift",type="latency"}"#,
        r#"rift_faults_injected_total{rule_id="21502",source="script",type="error"}"#,
        r#"rift_error_status_total{rule_id="21500",status="503"}"#,
        r#"rift_script_errors_total{error_type="runtime",rule_id="21503"}"#,
        r#"rift_flow_state_ops_total{operation="increment",result="success"}"#,
        // Materialised at listener start, so it is present at 0 without provoking a failure —
        // which is the property the docs promise and the reason the row-count guard above does
        // not have to be taught about a family that only appears under fault.
        r#"rift_preface_failures_total{kind="eof",listener="imposter"}"#,
        r#"rift_preface_failures_total{kind="io",listener="imposter"}"#,
        r#"rift_preface_failures_total{kind="timeout",listener="imposter"}"#,
    ] {
        assert!(
            scrape.contains(expected),
            "expected series `{expected}` in the scrape — the family may be present under the \
             wrong labels.\n{scrape}"
        );
    }

    // `source="v1"` was emitted by the fault helpers before #999 and is not a documented value.
    // Its reappearance means a caller is counting a fault twice, once through a composite helper
    // and once directly — which inflates every `sum(rate(...))` built on this family.
    assert!(
        !scrape.contains(r#"source="v1""#),
        "`source=\"v1\"` is not a documented label value; a fault is being counted twice.\n{scrape}"
    );

    for port in [21500u16, 21501, 21502, 21503, 21504, 21505, 21506] {
        let _ = manager.delete_imposter(port).await;
    }
}

/// A probability roll that passes but yields a zero delay injects nothing, so it must not count.
/// `rift_faults_injected_total` promises faults *injected*, not rolls that succeeded — and the
/// guard that makes that true (`apply_latency && latency_delay_ms > 0`) is exactly the kind of
/// condition that regresses silently.
#[tokio::test]
async fn a_zero_millisecond_latency_roll_injects_nothing_and_is_not_counted() {
    let manager = ImposterManager::new();
    mk(
        &manager,
        serde_json::json!({
            "port": 21507, "protocol": "http", "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "ok" },
                "_rift": { "fault": { "latency": { "probability": 1.0, "ms": 0 } } }
            }] }]
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    hit(&client, 21507, "/x").await;

    let scrape = rift_http_proxy::extensions::collect_metrics();
    assert!(
        !scrape.contains(r#"rule_id="21507""#),
        "a latency fault whose delay is 0ms injects nothing and must not be counted as an \
         injected fault.\n{scrape}"
    );

    let _ = manager.delete_imposter(21507).await;
}

/// The two families #999 deliberately removed must not come back by way of the docs table: a row
/// with no writer is exactly the defect this issue closes.
#[test]
fn dropped_families_are_not_documented() {
    let families = documented_families();
    for dropped in ["rift_active_flows", "rift_proxy_request_duration_ms"] {
        assert!(
            !families.iter().any(|f| f == dropped),
            "`{dropped}` was removed in #999 because the imposter path has no counterpart for it, \
             but it is still listed in docs/features/metrics.md"
        );
    }
}
