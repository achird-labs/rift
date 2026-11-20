//! Issue #1751: `_rift.script` with `engine: "wasm"` through the real doors — a component read from
//! `--scripts-dir`, admission refusals, the deadline, fuel and memory limits on the wire, a flow
//! store shared with Rhai, and a saved imposter that replays without its file.
#![cfg(feature = "wasm")]

#[path = "../../rift-mock-core/test-support/wasm_fixtures.rs"]
mod wasm_fixtures;

use clap::Parser;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, RunningServer, ServerBuilder};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn start(extra: &[&str]) -> (RunningServer, String) {
    let mut argv = vec![
        "rift",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--allow-injection",
    ];
    argv.extend_from_slice(extra);
    let server = ServerBuilder::from_cli(Cli::try_parse_from(argv).expect("cli"))
        .start()
        .await
        .expect("server starts");
    let admin = format!("http://{}", server.admin_addr());
    (server, admin)
}

async fn post_imposter(admin: &str, body: &Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{admin}/imposters"))
        .json(body)
        .send()
        .await
        .expect("admin request");
    let status = resp.status().as_u16();
    let text = resp.text().await.expect("body");
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn scripts_dir(files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, bytes) in files {
        std::fs::write(dir.path().join(name), bytes).expect("write component");
    }
    dir
}

fn wasm_stub(path: &str, script: Value) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }],
            "responses": [{ "_rift": { "script": script } }] })
}

// A component in --scripts-dir, its engine inferred from `.wasm`, answers; the imposter carries the
// component as base64 `code`, so the file is read once, at admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scripts_dir_component_answers_and_is_carried_as_base64() {
    let dir = scripts_dir(&[("ok.wasm", wasm_fixtures::respond_ok())]);
    let (server, admin) = start(&["--scripts-dir", dir.path().to_str().expect("utf8")]).await;
    let (status, body) = post_imposter(
        &admin,
        &json!({ "protocol": "http", "stubs": [wasm_stub("/ok", json!({ "file": "ok.wasm" }))] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let port = body["port"].as_u64().expect("port");
    let script = &body["stubs"][0]["responses"][0]["_rift"]["script"];
    assert_eq!(script["engine"], "wasm");
    assert_eq!(
        script["code"],
        wasm_fixtures::b64(&wasm_fixtures::respond_ok())
    );
    assert!(script.get("file").is_none(), "{script}");

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/ok"))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-rift-script"], "wasm");
    assert_eq!(resp.headers()["x-twin"], "yes");
    assert_eq!(resp.text().await.expect("body"), "ok");
    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_refuses_a_sockets_import_and_inline_source_with_400() {
    let dir = scripts_dir(&[("sock.wasm", wasm_fixtures::importing("sockets"))]);
    let (server, admin) = start(&["--scripts-dir", dir.path().to_str().expect("utf8")]).await;

    let (status, body) = post_imposter(
        &admin,
        &json!({ "protocol": "http", "stubs": [wasm_stub("/s", json!({ "file": "sock.wasm" }))] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string()
            .contains("wasm component imports wasi:sockets/tcp@0.2.6, which Rift does not provide"),
        "{body}"
    );

    let (status, body) = post_imposter(
        &admin,
        &json!({ "protocol": "http", "stubs": [wasm_stub("/i",
            json!({ "engine": "wasm", "code": "fn respond(ctx) { pass() }" }))] }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string()
            .contains(r#"engine \"wasm\" has no inline form; use file"#),
        "{body}"
    );
    drop(server);
}

// What GET /imposters returns is a complete imposter: posted to a server with no --scripts-dir, the
// base64 component still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_imposter_replays_without_its_file() {
    let dir = scripts_dir(&[("ok.wasm", wasm_fixtures::respond_ok())]);
    let (first, admin) = start(&["--scripts-dir", dir.path().to_str().expect("utf8")]).await;
    let (status, created) = post_imposter(
        &admin,
        &json!({ "protocol": "http", "stubs": [wasm_stub("/ok", json!({ "file": "ok.wasm" }))] }),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    drop(first);

    let (second, admin) = start(&[]).await;
    let mut saved = created.clone();
    saved.as_object_mut().expect("object").remove("port");
    let (status, body) = post_imposter(&admin, &saved).await;
    assert_eq!(status, 201, "{body}");
    let port = body["port"].as_u64().expect("port");
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/ok"))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "ok");
    drop(second);
}

fn cfg(v: Value) -> ImposterConfig {
    serde_json::from_value(v).expect("imposter config")
}

fn code(bytes: &[u8]) -> String {
    wasm_fixtures::b64(bytes)
}

// A spinning component times out with 504 + x-rift-script-timeout near its deadline, and a sibling
// stub on the same imposter answers while it spins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spinning_component_times_out_and_its_imposter_stays_responsive() {
    let manager = Arc::new(ImposterManager::new());
    let port = manager
        .create_imposter(cfg(json!({
            "protocol": "http",
            "_rift": { "scriptEngine": { "timeoutMs": 300 } },
            "stubs": [
                wasm_stub("/spin", json!({ "engine": "wasm", "code": code(&wasm_fixtures::spin()) })),
                { "predicates": [{ "equals": { "path": "/ok" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "fine" } }] }
            ]
        })))
        .await
        .expect("create");
    let spin = tokio::spawn(async move {
        let start = Instant::now();
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/spin"))
            .await
            .expect("spin request");
        (start.elapsed(), resp)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let during = reqwest::get(format!("http://127.0.0.1:{port}/ok"))
        .await
        .expect("sibling");
    assert_eq!(during.status(), 200);
    let (elapsed, resp) = spin.await.expect("join");
    assert_eq!(resp.status(), 504);
    assert_eq!(resp.headers()["x-rift-script-timeout"], "true");
    assert_eq!(resp.headers()["x-rift-script-error"], "true");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    let after = reqwest::get(format!("http://127.0.0.1:{port}/ok"))
        .await
        .expect("after");
    assert_eq!(after.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuel_and_memory_limits_answer_500_naming_the_limit() {
    let manager = Arc::new(ImposterManager::new());
    let port = manager
        .create_imposter(cfg(json!({
            "protocol": "http",
            "_rift": { "scriptEngine": { "wasm": { "fuel": 1000 } } },
            "stubs": [
                wasm_stub("/fuel", json!({ "engine": "wasm", "code": code(&wasm_fixtures::spin()) }))
            ]
        })))
        .await
        .expect("create");
    // Its own imposter: a fuel budget would also stop the growth before the memory cap does.
    let grow_port = manager
        .create_imposter(cfg(json!({
            "protocol": "http",
            "_rift": { "scriptEngine": { "wasm": { "maxMemoryMiB": 32 } } },
            "stubs": [
                wasm_stub("/grow", json!({ "engine": "wasm", "code": code(&wasm_fixtures::grow_1gib()) }))
            ]
        })))
        .await
        .expect("create");
    let fuel = reqwest::get(format!("http://127.0.0.1:{port}/fuel"))
        .await
        .expect("fuel");
    assert_eq!(fuel.status(), 500);
    assert_eq!(fuel.headers()["x-rift-script-error"], "true");
    let body = fuel.text().await.expect("body");
    assert!(body.contains("fuel exhausted (1000 units)"), "{body}");

    let grow = reqwest::get(format!("http://127.0.0.1:{grow_port}/grow"))
        .await
        .expect("grow");
    assert_eq!(grow.status(), 500);
    let body = grow.text().await.expect("body");
    assert!(
        body.contains("memory limit exceeded (maxMemoryMiB 32)"),
        "{body}"
    );

    let metrics = rift_http_proxy::extensions::metrics::collect_metrics();
    assert!(
        metrics.contains("rift_wasm_traps_total{kind=\"fuel\"}"),
        "{metrics}"
    );
    assert!(
        metrics.contains("rift_wasm_traps_total{kind=\"memory\"}"),
        "{metrics}"
    );
}

// The component and a Rhai script on one imposter read and write the same flow store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_and_a_rhai_script_share_the_flow_store() {
    let manager = Arc::new(ImposterManager::new());
    let port = manager
        .create_imposter(cfg(json!({
            "protocol": "http",
            "_rift": { "flowState": {} },
            "stubs": [
                wasm_stub("/incr", json!({ "engine": "wasm", "code": code(&wasm_fixtures::incr_count()) })),
                wasm_stub("/read", json!({ "engine": "rhai",
                    "code": "fn respond(ctx) { http(200, `${ctx.state.get(\"count\")}`) }" }))
            ]
        })))
        .await
        .expect("create");
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let incr = client
            .get(format!("http://127.0.0.1:{port}/incr"))
            .header("x-flow-id", "shared")
            .send()
            .await
            .expect("incr");
        assert_eq!(incr.status(), 200);
    }
    let read = client
        .get(format!("http://127.0.0.1:{port}/read"))
        .header("x-flow-id", "shared")
        .send()
        .await
        .expect("read");
    assert_eq!(read.status(), 200);
    assert_eq!(read.text().await.expect("body"), "2");
}

// `rift script check` on a `.wasm` file admits a component and names a refused import.
#[test]
fn script_check_admits_a_component_and_refuses_a_sockets_import() {
    use rift_http_proxy::script_cli::run_check;
    let dir = scripts_dir(&[
        ("ok.wasm", wasm_fixtures::respond_ok()),
        ("sock.wasm", wasm_fixtures::importing("sockets")),
    ]);
    let ok = run_check(&dir.path().join("ok.wasm"), "respond", false).expect("check ok");
    assert!(ok.is_ok(), "{:?}", ok.errors);
    let sock = run_check(&dir.path().join("sock.wasm"), "respond", false).expect("check sock");
    assert_eq!(sock.errors.len(), 1, "{:?}", sock.errors);
    assert!(
        sock.errors[0].contains("wasm component imports wasi:sockets/tcp@0.2.6"),
        "{:?}",
        sock.errors
    );
}

// `rift script run` runs a component and prints its decision; the state it wrote is reported.
#[test]
fn script_run_prints_a_components_decision_and_state() {
    use rift_http_proxy::script_cli::run_run;
    let dir = scripts_dir(&[
        ("ok.wasm", wasm_fixtures::respond_ok()),
        ("incr.wasm", wasm_fixtures::incr_count()),
    ]);
    let ok = run_run(&dir.path().join("ok.wasm"), None, &[], "f", None, "respond").expect("run");
    assert_eq!(ok.error, None);
    assert!(ok.decision.starts_with("http(200)"), "{}", ok.decision);
    assert!(ok.decision.ends_with("body=\"ok\""), "{}", ok.decision);

    let incr = run_run(
        &dir.path().join("incr.wasm"),
        None,
        &["count=41".to_string()],
        "f",
        Some("wasm"),
        "respond",
    )
    .expect("run incr");
    assert_eq!(incr.decision, "pass()");
    assert_eq!(incr.state, vec![("count".to_string(), json!(42))]);
}

// The conformance fixtures (feature 23) carry components as base64 `code`. They are generated from
// the same builder these tests use; `REGENERATE_WASM_FIXTURES=1` rewrites them.
#[test]
fn conformance_wasm_fixtures_match_the_builder() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/rift-fixtures/scripting");
    let stub = |path: &str, bytes: Vec<u8>| {
        wasm_stub(path, json!({ "engine": "wasm", "code": code(&bytes) }))
    };
    let fixtures = [
        (
            "wasm.json",
            json!({
                "protocol": "http",
                "name": "scripting · wasm components",
                "_rift": { "flowState": {} },
                "stubs": [
                    stub("/ok", wasm_fixtures::respond_ok()),
                    stub("/incr", wasm_fixtures::incr_count()),
                    wasm_stub("/read", json!({ "engine": "rhai",
                        "code": "fn respond(ctx) { http(200, `${ctx.state.get(\"count\")}`) }" }))
                ]
            }),
        ),
        (
            "wasm-sockets.json",
            json!({
                "protocol": "http",
                "name": "scripting · wasm component importing wasi:sockets",
                "stubs": [stub("/s", wasm_fixtures::importing("sockets"))]
            }),
        ),
    ];
    let regenerate = std::env::var_os("REGENERATE_WASM_FIXTURES").is_some();
    for (name, doc) in fixtures {
        let path = dir.join(name);
        let want = serde_json::to_string_pretty(&doc).expect("json") + "\n";
        if regenerate {
            std::fs::write(&path, &want).expect("write fixture");
        }
        let have = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; rerun with REGENERATE_WASM_FIXTURES=1",
                path.display()
            )
        });
        assert_eq!(
            have, want,
            "{name} is stale; rerun with REGENERATE_WASM_FIXTURES=1"
        );
    }
}

// A --datadir engine persists the component in the imposter's file, so a restart with no
// --scripts-dir serves it. (Both engines share this process's compile cache, so this pins the
// file's content and the serve; the cold-cache compile is a unit test.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_datadir_imposter_serves_its_component_after_a_restart() {
    let scripts = scripts_dir(&[("ok.wasm", wasm_fixtures::respond_ok())]);
    let data = tempfile::tempdir().expect("datadir");
    let datadir = data.path().to_str().expect("utf8");
    let (first, admin) = start(&[
        "--datadir",
        datadir,
        "--scripts-dir",
        scripts.path().to_str().expect("utf8"),
    ])
    .await;
    let (status, created) = post_imposter(
        &admin,
        &json!({ "protocol": "http", "stubs": [wasm_stub("/ok", json!({ "file": "ok.wasm" }))] }),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let port = created["port"].as_u64().expect("port");
    first.shutdown().await;

    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(data.path().join(format!("{port}.json"))).expect("saved file"),
    )
    .expect("json");
    let script = &saved["stubs"][0]["responses"][0]["_rift"]["script"];
    assert_eq!(script["engine"], "wasm");
    assert_eq!(script["code"], code(&wasm_fixtures::respond_ok()));
    assert!(script.get("file").is_none(), "{script}");

    let (second, _) = start(&["--datadir", datadir]).await;
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/ok"))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "ok");
    second.shutdown().await;
}
