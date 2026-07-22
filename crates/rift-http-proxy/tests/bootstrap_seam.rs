//! Issue #807: `--rcfile`, `stop`, `restart` and `save` were private functions in the `rift`
//! binary's `main.rs`, so an alternative binary (rift-enterprise's `rift-ee-server`) could only
//! reach them by copy-paste — forking behaviour that is meant to stay shared.
//!
//! This suite is deliberately an *integration* test: it compiles as an external crate, so it fails
//! to build if the seam is not genuinely public. That is the property the issue is about.

use clap::Parser;
use rift_http_proxy::bootstrap;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::sync::Arc;

fn cli(args: &[&str]) -> Cli {
    let mut argv = vec!["rift"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("cli parse")
}

fn write_rcfile(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("rift.rc");
    std::fs::write(&path, body).expect("write rcfile");
    path
}

// AC1: an rcfile fills only the fields still at their clap defaults.
#[test]
fn rcfile_fills_fields_left_at_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = write_rcfile(
        &dir,
        r#"{"port": 4321, "host": "127.0.0.1", "logLevel": "debug",
            "allowInjection": true, "localOnly": true,
            "datadir": "/tmp/rc-datadir", "configfile": "/tmp/rc-config.json"}"#,
    );

    let mut parsed = cli(&[]);
    bootstrap::apply_rcfile_defaults(&mut parsed, &rcfile).expect("rcfile applies");

    assert_eq!(parsed.port, 4321);
    assert_eq!(parsed.host, "127.0.0.1");
    assert_eq!(parsed.loglevel, "debug");
    assert!(parsed.allow_injection);
    assert!(parsed.local_only);
    assert_eq!(
        parsed.datadir.as_deref(),
        Some(std::path::Path::new("/tmp/rc-datadir"))
    );
    assert_eq!(
        parsed.configfile.as_deref(),
        Some(std::path::Path::new("/tmp/rc-config.json"))
    );
}

// AC2: an explicitly-supplied flag always beats the rcfile — the rule that would silently rot if a
// downstream binary reimplemented this by hand.
#[test]
fn rcfile_never_overrides_an_explicit_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = write_rcfile(&dir, r#"{"port": 4321, "host": "10.0.0.1"}"#);

    let mut parsed = cli(&["--port", "9999", "--host", "192.168.0.1"]);
    bootstrap::apply_rcfile_defaults(&mut parsed, &rcfile).expect("rcfile applies");

    assert_eq!(
        parsed.port, 9999,
        "explicit --port must win over the rcfile"
    );
    assert_eq!(
        parsed.host, "192.168.0.1",
        "explicit --host must win over the rcfile"
    );
}

// AC3: an unrecognised key is ignored (warned), not fatal — and the recognised keys beside it
// still apply.
#[test]
fn rcfile_unknown_key_is_ignored_without_dropping_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = write_rcfile(&dir, r#"{"nopeNotAKey": 1, "port": 4321}"#);

    let mut parsed = cli(&[]);
    bootstrap::apply_rcfile_defaults(&mut parsed, &rcfile).expect("unknown keys are not fatal");
    assert_eq!(parsed.port, 4321);
}

// AC4: a structurally wrong rcfile is an error, not a silent no-op.
#[test]
fn rcfile_non_object_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = write_rcfile(&dir, r#"["not", "an", "object"]"#);

    let mut parsed = cli(&[]);
    let err = bootstrap::apply_rcfile_defaults(&mut parsed, &rcfile)
        .expect_err("a non-object rcfile must be rejected");
    assert!(
        err.to_string().contains("object"),
        "the error should name the expected shape, got: {err}"
    );
}

#[test]
fn rcfile_missing_file_is_an_error() {
    let mut parsed = cli(&[]);
    let err = bootstrap::apply_rcfile_defaults(&mut parsed, std::path::Path::new("/nope/rift.rc"))
        .expect_err("a missing rcfile must be reported");
    assert!(!err.to_string().is_empty());
}

// AC5: stop_server reports a missing pidfile rather than exiting successfully.
#[test]
fn stop_server_missing_pidfile_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = bootstrap::stop_server(&dir.path().join("absent.pid"))
        .expect_err("a missing pidfile must be an error");
    assert!(err.to_string().contains("PID file not found"), "got: {err}");
}

#[test]
fn stop_server_unparseable_pid_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("garbage.pid");
    std::fs::write(&pidfile, "not-a-pid").expect("write pidfile");

    bootstrap::stop_server(&pidfile).expect_err("a non-numeric pidfile must be an error");
    assert!(
        pidfile.exists(),
        "a pidfile that was never acted on must not be removed"
    );
}

// AC6: the happy path — stop_server signals the process and clears the pidfile.
#[cfg(unix)]
#[test]
fn stop_server_signals_the_process_and_removes_the_pidfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn a stand-in server process");
    let pidfile = dir.path().join("server.pid");
    std::fs::write(&pidfile, child.id().to_string()).expect("write pidfile");

    bootstrap::stop_server(&pidfile).expect("stop_server succeeds");

    assert!(!pidfile.exists(), "stop_server must remove the pidfile");
    let status = child.wait().expect("reap the stand-in process");
    assert!(
        !status.success(),
        "the process should have been terminated by the signal, got: {status:?}"
    );
}

// AC7: save_imposters fetches the replayable config from a live admin API and writes it.
#[tokio::test]
async fn save_imposters_writes_the_replayable_config() {
    let manager = Arc::new(ImposterManager::new());
    let imposter: ImposterConfig = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "port": 0,
        "name": "bootstrap-seam",
        "stubs": [{
            "predicates": [{"equals": {"path": "/ping"}}],
            "responses": [{"is": {"statusCode": 200, "body": "pong"}}]
        }]
    }))
    .expect("imposter config");
    manager
        .create_imposter(imposter)
        .await
        .expect("create imposter");

    let server = ServerBuilder::from_cli(cli(&["--host", "127.0.0.1", "--port", "0"]))
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("admin API starts");
    let addr = server.admin_addr();

    let dir = tempfile::tempdir().expect("tempdir");
    let savefile = dir.path().join("imposters.json");
    let (host, port) = (addr.ip().to_string(), addr.port());
    let saved = tokio::task::spawn_blocking(move || {
        bootstrap::save_imposters(&host, port, &savefile, false).map(|()| savefile)
    })
    .await
    .expect("save task joins");
    let savefile = saved.expect("save_imposters succeeds");

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&savefile).expect("read savefile"))
            .expect("savefile is JSON");
    let names: Vec<&str> = written["imposters"]
        .as_array()
        .expect("imposters array")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        names.contains(&"bootstrap-seam"),
        "the saved config must contain the live imposter, got: {written}"
    );

    server.shutdown().await;
}

// AC1 (issue #815): the regression gate for the nested-runtime panic — `save_imposters_async` is
// called **directly** from an async worker thread (no `spawn_blocking`). This is the call that
// panics with "Cannot start a runtime from within a runtime" if the async body is not exposed.
#[tokio::test]
async fn save_imposters_async_works_from_async_context() {
    let manager = Arc::new(ImposterManager::new());
    let imposter: ImposterConfig = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "port": 0,
        "name": "async-seam",
        "stubs": [{
            "predicates": [{"equals": {"path": "/ping"}}],
            "responses": [{"is": {"statusCode": 200, "body": "pong"}}]
        }]
    }))
    .expect("imposter config");
    manager
        .create_imposter(imposter)
        .await
        .expect("create imposter");

    let server = ServerBuilder::from_cli(cli(&["--host", "127.0.0.1", "--port", "0"]))
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("admin API starts");
    let addr = server.admin_addr();

    let dir = tempfile::tempdir().expect("tempdir");
    let savefile = dir.path().join("imposters.json");
    bootstrap::save_imposters_async(&addr.ip().to_string(), addr.port(), &savefile, false)
        .await
        .expect("save_imposters_async succeeds from an async context");

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&savefile).expect("read savefile"))
            .expect("savefile is JSON");
    let names: Vec<&str> = written["imposters"]
        .as_array()
        .expect("imposters array")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        names.contains(&"async-seam"),
        "the saved config must contain the live imposter, got: {written}"
    );

    server.shutdown().await;
}

// AC3 (issue #815): `remove_proxies=true` must reach the admin API through the async form — a
// proxy-only stub is dropped from the saved config, a normal stub is kept. Proves the query param
// is appended and honored, not just that the call succeeds.
#[tokio::test]
async fn save_imposters_async_remove_proxies_strips_proxy_stubs() {
    let manager = Arc::new(ImposterManager::new());
    let imposter: ImposterConfig = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "port": 0,
        "name": "proxy-seam",
        "stubs": [
            {
                "predicates": [{"equals": {"path": "/kept"}}],
                "responses": [{"is": {"statusCode": 200, "body": "kept"}}]
            },
            {
                "responses": [{"proxy": {
                    "to": "http://127.0.0.1:1", "mode": "proxyOnce"
                }}]
            }
        ]
    }))
    .expect("imposter config");
    manager
        .create_imposter(imposter)
        .await
        .expect("create imposter");

    let server = ServerBuilder::from_cli(cli(&["--host", "127.0.0.1", "--port", "0"]))
        .manager(Arc::clone(&manager))
        .start()
        .await
        .expect("admin API starts");
    let addr = server.admin_addr();
    let (host, port) = (addr.ip().to_string(), addr.port());
    let dir = tempfile::tempdir().expect("tempdir");

    // remove_proxies=false: the proxy stub survives.
    let with_proxy = dir.path().join("with-proxy.json");
    bootstrap::save_imposters_async(&host, port, &with_proxy, false)
        .await
        .expect("save without removeProxies");
    let with_proxy_raw = std::fs::read_to_string(&with_proxy).expect("read with-proxy");
    assert!(
        with_proxy_raw.contains("\"proxy\""),
        "without removeProxies the proxy stub must be present, got: {with_proxy_raw}"
    );

    // remove_proxies=true: the proxy-only stub is stripped, the normal stub stays.
    let stripped = dir.path().join("stripped.json");
    bootstrap::save_imposters_async(&host, port, &stripped, true)
        .await
        .expect("save with removeProxies");
    let stripped_raw = std::fs::read_to_string(&stripped).expect("read stripped");
    assert!(
        !stripped_raw.contains("\"proxy\""),
        "removeProxies=true must strip the proxy-only stub, got: {stripped_raw}"
    );
    assert!(
        stripped_raw.contains("/kept"),
        "removeProxies=true must keep the non-proxy stub, got: {stripped_raw}"
    );

    server.shutdown().await;
}
