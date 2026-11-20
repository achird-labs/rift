//! Issue #1759: `rift wasm check` replays a saved imposter against a component, offline.
#![cfg(feature = "wasm")]

use clap::Parser;
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use rift_http_proxy::wasm_cli::{CheckOptions, run_check};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[path = "../../rift-mock-core/test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

const RIFT: &str = env!("CARGO_BIN_EXE_rift-http-proxy");

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write");
    path
}

fn options(component: &Path, corpus: &Path) -> CheckOptions {
    CheckOptions {
        component: component.to_owned(),
        corpus: corpus.to_owned(),
        port: None,
        export: None,
        knobs: None,
        seed: None,
        fields: None,
        stop_on_first: false,
    }
}

/// `respond_ok` answers `200` `ok` (with `x-twin: yes`) to everything.
fn stub(path: &str, status: u16, body: &str) -> Value {
    json!({ "predicates": [{ "equals": { "method": "GET", "path": path } }],
            "responses": [{ "is": { "statusCode": status, "body": body } }] })
}

fn corpus(stubs: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({ "port": 4545, "protocol": "http", "stubs": stubs })).expect("json")
}

#[tokio::test]
async fn a_matching_stub_passes_a_drifting_one_is_named_and_others_are_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let corpus = write(
        dir.path(),
        "saved.json",
        &corpus(vec![
            stub("/a", 200, "ok"),
            stub("/b", 200, "not ok"),
            json!({ "responses": [{ "proxy": { "to": "http://upstream" } }] }),
        ]),
    );
    let report = run_check(&options(&component, &corpus))
        .await
        .expect("runs");
    assert_eq!(
        (report.stubs, report.checked, report.skipped, report.drifted),
        (3, 2, 1, 1)
    );
    let drift = &report.drifts[0];
    assert_eq!(drift.stub_index, 1);
    assert_eq!(drift.mismatches.len(), 1);
    assert_eq!(drift.mismatches[0].field, "body");
    assert_eq!(drift.mismatches[0].expected, json!("not ok"));
    assert_eq!(drift.mismatches[0].actual, json!("ok"));
    assert_eq!(report.skips[0].stub_index, 2);

    // Comparing the status alone, the same corpus passes.
    let mut status_only = options(&component, &corpus);
    status_only.fields = Some("status".into());
    assert!(run_check(&status_only).await.expect("runs").is_ok());
    // A header is compared only when named.
    let mut with_header = options(&component, &corpus);
    with_header.fields = Some("headers.x-twin,status".into());
    let report = run_check(&with_header).await.expect("runs");
    assert!(!report.is_ok(), "the recording has no x-twin header");
    assert!(
        report
            .drifts
            .iter()
            .all(|d| d.mismatches[0].field == "headers.x-twin")
    );
}

// The clock is frozen at each stub's `recordedAt`, and a stub without one keeps the instant before
// it. `wall_clock_seconds` answers `delay(<the seconds it reads>)`, which drifts on `decision` and
// so shows what the component saw.
#[tokio::test]
async fn each_stub_runs_on_its_recorded_clock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(
        dir.path(),
        "clock.wasm",
        &wasm_fixtures::wall_clock_seconds(),
    );
    let mut first = stub("/a", 200, "ok");
    first["recordedAt"] = json!(1_700_000_000);
    let second = stub("/b", 200, "ok");
    let mut third = stub("/c", 200, "ok");
    third["recordedAt"] = json!(1_700_000_042);
    let corpus_path = write(
        dir.path(),
        "saved.json",
        &corpus(vec![first, second, third]),
    );
    let report = run_check(&options(&component, &corpus_path))
        .await
        .expect("runs");
    let seen: Vec<Value> = report
        .drifts
        .iter()
        .map(|d| d.mismatches[0].actual.clone())
        .collect();
    assert_eq!(
        seen,
        [
            json!("delay(1700000000)"),
            json!("delay(1700000000)"),
            json!("delay(1700000042)")
        ]
    );

    let mut bad = stub("/a", 200, "ok");
    bad["recordedAt"] = json!("yesterday");
    let corpus_path = write(dir.path(), "bad.json", &corpus(vec![bad]));
    let err = run_check(&options(&component, &corpus_path))
        .await
        .expect_err("a bad recordedAt");
    assert!(format!("{err:#}").contains("recordedAt"), "{err:#}");
}

// A `proxyAlways` stub holds one recorded response per call: each replays, in order.
#[tokio::test]
async fn every_recorded_response_of_a_stub_replays() {
    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let always = json!({ "predicates": [{ "equals": { "path": "/a" } }], "responses": [
        { "is": { "statusCode": 200, "body": "ok" } },
        { "is": { "statusCode": 200, "body": "second" } },
        { "is": { "statusCode": 200, "body": "ok" } } ] });
    let corpus_path = write(dir.path(), "saved.json", &corpus(vec![always]));
    let report = run_check(&options(&component, &corpus_path))
        .await
        .expect("runs");
    assert_eq!((report.checked, report.drifted), (3, 1), "{report:#?}");
    assert_eq!(
        (report.drifts[0].stub_index, report.drifts[0].response_index),
        (0, 1)
    );
    // --stop-on-first stops at the first drift.
    let two = write(
        dir.path(),
        "two.json",
        &corpus(vec![stub("/a", 500, "ok"), stub("/b", 500, "ok")]),
    );
    let mut first_only = options(&component, &two);
    first_only.stop_on_first = true;
    let report = run_check(&first_only).await.expect("runs");
    assert_eq!((report.checked, report.drifted), (1, 1));
}

// Nothing checked is not a pass: an all-skipped corpus, or a document that is not a saved imposter,
// is an error. A binary recording is skipped with its reason.
#[tokio::test]
async fn a_run_that_checks_nothing_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let proxies = write(
        dir.path(),
        "proxies.json",
        &corpus(vec![
            json!({ "responses": [{ "proxy": { "to": "http://upstream" } }] }),
        ]),
    );
    let err = run_check(&options(&component, &proxies))
        .await
        .expect_err("all skipped");
    assert!(format!("{err:#}").contains("nothing to check"), "{err:#}");
    let not_a_corpus = write(
        dir.path(),
        "package.json",
        br#"{"name": "app", "version": "1.0.0"}"#,
    );
    let err = run_check(&options(&component, &not_a_corpus))
        .await
        .expect_err("no stubs");
    assert!(format!("{err:#}").contains("no \"stubs\" array"), "{err:#}");

    let binary = json!({ "predicates": [{ "equals": { "path": "/bin" } }], "responses": [
        { "is": { "statusCode": 200, "body": "AAEC", "_mode": "binary" } } ] });
    let corpus_path = write(
        dir.path(),
        "binary.json",
        &corpus(vec![binary, stub("/a", 200, "ok")]),
    );
    let report = run_check(&options(&component, &corpus_path))
        .await
        .expect("runs");
    assert_eq!((report.checked, report.skipped, report.drifted), (1, 1, 0));
    assert!(
        report.skips[0].reason.contains("binary"),
        "{:?}",
        report.skips
    );
}

#[tokio::test]
async fn a_component_that_does_not_answer_drifts_on_its_decision() {
    let dir = tempfile::tempdir().expect("tempdir");
    // `respond_bare(0)` answers `pass`: the stub's own response, which a twin must not rely on.
    let component = write(dir.path(), "pass.wasm", &wasm_fixtures::respond_bare(0));
    let corpus = write(
        dir.path(),
        "saved.json",
        &corpus(vec![stub("/a", 200, "ok")]),
    );
    let report = run_check(&options(&component, &corpus))
        .await
        .expect("runs");
    assert_eq!(report.drifted, 1);
    assert_eq!(report.drifts[0].mismatches[0].field, "decision");
    assert_eq!(report.drifts[0].mismatches[0].actual, json!("pass"));
}

#[tokio::test]
async fn errors_that_keep_the_check_from_running_are_errors_not_drift() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let module = write(dir.path(), "module.wasm", &wasm_fixtures::core_module());
    let corpus_path = write(
        dir.path(),
        "saved.json",
        &corpus(vec![stub("/a", 200, "ok")]),
    );
    let two = write(
        dir.path(),
        "two.json",
        &serde_json::to_vec(&json!({ "imposters": [
            { "port": 1, "protocol": "http", "stubs": [] },
            { "port": 2, "protocol": "http", "stubs": [
                { "predicates": [{ "equals": { "path": "/a" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] } ] } ] }))
        .expect("json"),
    );
    for (opts, why) in [
        (options(&module, &corpus_path), "not a component"),
        (
            options(&dir.path().join("missing.wasm"), &corpus_path),
            "cannot read the component",
        ),
        (
            options(&ok, &dir.path().join("missing.json")),
            "cannot read the corpus",
        ),
        (options(&ok, &two), "choose one with --port"),
        (
            CheckOptions {
                fields: Some("statuss".into()),
                ..options(&ok, &corpus_path)
            },
            "unknown field",
        ),
        (
            CheckOptions {
                export: Some("http".into()),
                ..options(&ok, &corpus_path)
            },
            "#1758",
        ),
    ] {
        let err = run_check(&opts).await.expect_err(why);
        assert!(format!("{err:#}").contains(why), "{err:#} lacks {why}");
    }
    // `--port` picks one imposter out of several.
    let mut picked = options(&ok, &two);
    picked.port = Some(2);
    assert_eq!(run_check(&picked).await.expect("runs").port, Some(2));
}

#[test]
fn the_cli_exits_0_1_and_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let pass = write(
        dir.path(),
        "pass.json",
        &corpus(vec![stub("/a", 200, "ok")]),
    );
    let drift = write(
        dir.path(),
        "drift.json",
        &corpus(vec![stub("/a", 500, "ok")]),
    );
    let run = |corpus: &Path, extra: &[&str]| {
        std::process::Command::new(RIFT)
            .args(["wasm", "check", "--component"])
            .arg(&ok)
            .arg("--corpus")
            .arg(corpus)
            .args(extra)
            .output()
            .expect("runs")
    };
    assert_eq!(run(&pass, &[]).status.code(), Some(0));
    let drifted = run(&drift, &["--json"]);
    assert_eq!(drifted.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&drifted.stdout).expect("--json is JSON");
    assert_eq!(report["drifted"], 1);
    assert_eq!(report["drifts"][0]["mismatches"][0]["field"], "status");
    assert_eq!(run(&pass, &["--fields", "nope"]).status.code(), Some(2));
}

async fn start() -> (RunningServer, String) {
    let cli = Cli::try_parse_from(["rift", "--host", "127.0.0.1", "--port", "0"]).expect("cli");
    let server = ServerBuilder::from_cli(cli)
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn post(url: &str, body: &Value) -> Value {
    let resp = reqwest::Client::new()
        .post(url)
        .json(body)
        .send()
        .await
        .expect("post");
    assert!(resp.status().is_success(), "{url}: {}", resp.status());
    resp.json().await.expect("json")
}

// The issue's end to end: record through a proxy, save, check. A recorded stub carries
// `recordedAt`, and the check replays it.
#[tokio::test]
async fn a_recording_saved_from_a_proxy_replays_against_a_component() {
    let (server, admin) = start().await;
    let upstream = post(
        &format!("{admin}/imposters"),
        &json!({ "protocol": "http", "stubs": [{ "responses": [
            { "is": { "statusCode": 200, "body": "ok" } } ] }] }),
    )
    .await["port"]
        .as_u64()
        .expect("port");
    let proxy = post(
        &format!("{admin}/imposters"),
        &json!({ "protocol": "http", "stubs": [{ "responses": [{ "proxy": {
            "to": format!("http://127.0.0.1:{upstream}"),
            "mode": "proxyOnce",
            "predicateGenerators": [{ "matches": { "method": true, "path": true } }] } }] }] }),
    )
    .await["port"]
        .as_u64()
        .expect("port");
    for path in ["/one", "/two"] {
        let resp = reqwest::get(format!("http://127.0.0.1:{proxy}{path}"))
            .await
            .expect("through the proxy");
        assert_eq!(resp.status(), 200);
    }
    let saved: Value = reqwest::get(format!("{admin}/imposters/{proxy}?replayable=true"))
        .await
        .expect("save")
        .json()
        .await
        .expect("json");
    let stubs = saved["stubs"].as_array().expect("stubs");
    let recorded: Vec<&Value> = stubs
        .iter()
        .filter(|s| s.get("recordedAt").is_some())
        .collect();
    assert_eq!(
        recorded.len(),
        2,
        "each recorded stub carries recordedAt: {saved:#}"
    );
    server.shutdown().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(dir.path(), "ok.wasm", &wasm_fixtures::respond_ok());
    let corpus = write(
        dir.path(),
        "saved.json",
        &serde_json::to_vec(&saved).expect("json"),
    );
    let report = run_check(&options(&component, &corpus))
        .await
        .expect("runs");
    assert_eq!((report.checked, report.drifted), (2, 0), "{report:#?}");
    assert_eq!(report.skipped, 1, "the proxy stub itself");

    // Edit one recorded answer: that stub, and only it, drifts.
    let mut edited = saved.clone();
    let index = edited["stubs"]
        .as_array()
        .expect("stubs")
        .iter()
        .position(|s| s.get("recordedAt").is_some())
        .expect("a recorded stub");
    edited["stubs"][index]["responses"][0]["is"]["body"] = json!("changed");
    let corpus = write(
        dir.path(),
        "edited.json",
        &serde_json::to_vec(&edited).expect("json"),
    );
    let report = run_check(&options(&component, &corpus))
        .await
        .expect("runs");
    assert_eq!(report.drifted, 1);
    assert_eq!(report.drifts[0].stub_index, index);
}

// Stateful and in order: the counter-twin example answers 409 above its limit, so the third
// recorded call matches only when the sequence replays in order. Needs the example built
// (`examples/wasm/counter-twin`, wasm32-wasip2); CI's wasm example job runs it.
#[tokio::test]
#[ignore = "needs examples/wasm/counter-twin built for wasm32-wasip2"]
async fn a_stateful_twin_matches_only_in_recorded_order() {
    let component = std::env::var_os("RIFT_COUNTER_TWIN").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../examples/wasm/counter-twin/target/wasm32-wasip2/release/counter_twin.wasm",
            )
        },
        PathBuf::from,
    );
    let recorded = |path: &str, status: u16, body: Value| {
        json!({ "predicates": [{ "equals": { "method": "GET", "path": path } }],
                "responses": [{ "is": { "statusCode": status, "body": body } }] })
    };
    let in_order = vec![
        recorded("/hit", 200, json!({ "count": 1 })),
        recorded("/hit", 200, json!({ "count": 2 })),
        recorded(
            "/hit",
            409,
            json!({ "error": "above limit", "count": 3, "limit": 2 }),
        ),
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = options(
        &component,
        &write(dir.path(), "a.json", &corpus(in_order.clone())),
    );
    opts.knobs = Some(r#"{"limit":2}"#.into());
    let report = run_check(&opts).await.expect("runs");
    assert!(report.is_ok(), "{report:#?}");

    let mut reordered = in_order;
    reordered.swap(0, 2);
    opts.corpus = write(dir.path(), "b.json", &corpus(reordered));
    let report = run_check(&opts).await.expect("runs");
    assert_eq!(report.drifted, 2, "{report:#?}");
}

// Issue #1795: the corpus's rngKey is honoured, so each stub's draws depend on its key, not on
// where it sits in the replay.
#[tokio::test]
async fn a_keyed_corpus_draws_per_stub_whatever_the_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let component = write(dir.path(), "random.wasm", &wasm_fixtures::random_body());
    let keyed = |stubs: Vec<Value>| {
        serde_json::to_vec(&json!({ "port": 4545, "protocol": "http",
            "_rift": { "scriptEngine": { "seed": 7, "rngKey": ["path"] } },
            "stubs": stubs }))
        .expect("json")
    };
    let stubs: Vec<Value> = ["/a", "/b", "/c"]
        .iter()
        .map(|p| stub(p, 200, "x"))
        .collect();
    let draws = |report: &rift_http_proxy::wasm_cli::CheckReport| -> std::collections::BTreeMap<String, Value> {
        report
            .drifts
            .iter()
            .map(|d| (format!("stub-{}", d.stub_index), d.mismatches[0].actual.clone()))
            .collect()
    };
    let forward = write(dir.path(), "f.json", &keyed(stubs.clone()));
    let first = run_check(&options(&component, &forward))
        .await
        .expect("runs");
    let mut reversed = stubs;
    reversed.reverse();
    let backward = write(dir.path(), "b.json", &keyed(reversed));
    let second = run_check(&options(&component, &backward))
        .await
        .expect("runs");
    let (f, b) = (draws(&first), draws(&second));
    // Stub 0 forward is stub 2 backward (/a), and so on.
    assert_eq!(f["stub-0"], b["stub-2"]);
    assert_eq!(f["stub-2"], b["stub-0"]);
    assert_ne!(f["stub-0"], f["stub-1"]);
}
