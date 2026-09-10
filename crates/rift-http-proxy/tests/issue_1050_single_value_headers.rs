//! Issue #1050: a single-valued header object that names one header twice is refused, and the
//! refusal reaches the user.
//!
//! The algorithm lives in `rift_types::wire::single_value_headers` and the serde wiring is pinned
//! next to the fields it is attached to. What is pinned *here* is the part those cannot show: that
//! the rejection actually surfaces on the two paths a user takes, with a message that says what to
//! do about it.
//!
//! That matters more than usual because this is a **breaking input change** — a document that
//! loaded before is refused now — so the promise in the CHANGELOG and in
//! `docs/mountebank/imposters.md` is a user-facing contract. A refactor that made
//! `StubResponseRaw::proxy` an untagged alternative, or routed `_rift` through a lenient `Value`
//! re-parse the way `_behaviors` already is, would collapse the custom message into "data did not
//! match any variant" — or drop the imposter silently — while every serde-layer test above still
//! passed.

use std::time::Duration;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_mock_core::imposter::ImposterManager;
use std::sync::Arc;

/// A stub whose `proxy.injectHeaders` names one header twice, differing only in case.
fn duplicate_inject_headers() -> serde_json::Value {
    serde_json::json!({
        "port": 22800,
        "protocol": "http",
        "stubs": [{"responses": [{"proxy": {
            "to": "http://127.0.0.1:1",
            "mode": "proxyOnce",
            "injectHeaders": {"x-trace": "a", "X-Trace": "b"}
        }}]}]
    })
}

#[tokio::test]
async fn posting_a_duplicate_named_header_is_refused_with_an_actionable_400() {
    let manager = Arc::new(ImposterManager::new());
    let admin = "127.0.0.1:12770";
    let server = AdminApiServer::new(admin.parse().unwrap(), manager.clone(), None);
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{admin}/imposters"))
        .json(&duplicate_inject_headers())
        .send()
        .await
        .expect("admin API reachable");

    let status = resp.status();
    let body = resp.text().await.expect("body");

    assert_eq!(
        status, 400,
        "a document naming one header twice must be refused, not accepted and then served as two \
         nondeterministically ordered header lines. body: {body}"
    );
    assert!(
        body.contains("names each header once"),
        "the refusal must say what to do about it, and name both spellings — otherwise the author \
         has a 400 and no idea which header. body: {body}"
    );
    assert!(
        body.contains("x-trace") && body.contains("X-Trace"),
        "both spellings must appear so the author can find them. body: {body}"
    );

    // And it is refused, not half-created: nothing bound.
    let listed = reqwest::get(format!("http://{admin}/imposters"))
        .await
        .expect("list")
        .text()
        .await
        .expect("body");
    assert!(
        !listed.contains("22800"),
        "a refused document must leave no imposter behind: {listed}"
    );
}

#[test]
fn loading_a_duplicate_named_header_from_a_config_file_fails_startup() {
    // The `--configfile` path. A startup error is the point: the alternative is booting with the
    // imposter silently absent, which looks like a working server serving nothing.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!([duplicate_inject_headers()]))
            .expect("serialize"),
    )
    .expect("write config");

    let err = load_configs(&ConfigSource::File {
        path,
        no_parse: true,
    })
    .expect_err("a config file naming one header twice must not load");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("names each header once"),
        "the startup error must carry the same actionable message as the 400, not a generic \
         parse failure: {msg}"
    );
}

#[test]
fn a_config_file_naming_each_header_once_still_loads() {
    // The anti-vacuity half: without this, a `load_configs` that failed on *everything* would
    // satisfy the test above.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imposters.json");
    let mut config = duplicate_inject_headers();
    config["stubs"][0]["responses"][0]["proxy"]["injectHeaders"] =
        serde_json::json!({"x-trace": "a"});
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!([config])).expect("serialize"),
    )
    .expect("write config");

    let loaded = load_configs(&ConfigSource::File {
        path,
        no_parse: true,
    })
    .expect("a well-formed config loads");
    assert_eq!(loaded.len(), 1);
}
