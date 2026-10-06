//! `--configfile` carries the intercept listener and its rules (issue #655).
//!
//! The property under test is the one the feature exists for: a single declarative file brings up
//! the listener **with its rules already installed**, so a container needs no bootstrap sidecar and
//! no `POST /intercept/rules`. Every test here therefore asserts against a server started from a
//! config file alone — the only admin call any of them makes is `GET /intercept/ca.pem`, to obtain
//! the trust anchor a real SUT would get from a mounted CA file.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::io::Write;
use std::path::{Path, PathBuf};

fn write_config(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("config.json");
    let mut f = std::fs::File::create(&path).expect("create config");
    f.write_all(body.as_bytes()).expect("write config");
    path
}

fn cli_with_config(path: &Path, extra: &[&str]) -> Cli {
    let mut args = vec![
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--configfile",
        path.to_str().expect("utf-8 path"),
    ];
    args.extend_from_slice(extra);
    Cli::parse_from(args)
}

/// Start expecting a startup abort, returning the rendered error chain. `RunningServer` is not
/// `Debug`, so `expect_err` is unavailable; a server that starts when it must not is shut down
/// before failing, so a broken gate cannot leave a listener bound for the rest of the suite.
async fn start_expecting_error(cli: Cli, why: &str) -> String {
    match ServerBuilder::from_cli(cli).start().await {
        Ok(server) => {
            server.shutdown().await;
            panic!("{why}");
        }
        Err(e) => format!("{e:#}"),
    }
}

/// A client that trusts only the intercept CA and proxies HTTPS through the listener — the SUT.
fn sut_client(intercept: std::net::SocketAddr, ca_pem: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{intercept}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap()
}

/// AC1/AC2 headline: listener up and rule installed from the file alone. No `POST /intercept`,
/// no `POST /intercept/rules` — the bootstrap container this issue deletes.
#[tokio::test]
async fn configfile_intercept_block_serves_without_any_admin_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"{
            "imposters": [],
            "intercept": {
                "port": 0,
                "rules": [
                    { "host": "cdn.example.com",
                      "predicates": [{ "equals": { "path": "/datafiles/key-a.json" } }],
                      "action": { "serve": { "statusCode": 200,
                                             "headers": { "content-type": "application/json" },
                                             "body": "{\"featureX\":\"ON\"}" } } },
                    { "host": "cdn.example.com",
                      "predicates": [{ "equals": { "path": "/datafiles/key-b.json" } }],
                      "action": { "serve": { "statusCode": 200,
                                             "headers": { "content-type": "application/json" },
                                             "body": { "featureX": "ON" } } } },
                    { "host": "cdn.example.com",
                      "predicates": [{ "equals": { "path": "/datafiles/key-c.json" } }],
                      "action": { "serve": { "statusCode": "418",
                                             "headers": { "content-type": "application/json",
                                                          "set-cookie": ["a=1", "b=2"] },
                                             "body": { "featureX": "ON" } } } }
                ]
            }
        }"#,
    );

    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts with an intercept block");
    let intercept = server
        .intercept_addr()
        .expect("the block binds the listener without --intercept-port");

    let ca_pem = reqwest::get(format!("http://{}/intercept/ca.pem", server.admin_addr()))
        .await
        .expect("ca.pem")
        .text()
        .await
        .unwrap();

    let resp = sut_client(intercept, &ca_pem)
        .get("https://cdn.example.com/datafiles/key-a.json")
        .send()
        .await
        .expect("the SUT's hard-coded HTTPS call is intercepted");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), r#"{"featureX":"ON"}"#);

    // Issue #933: a `serve` body declared as a JSON *object* survives the whole boot path — parsed
    // out of the config file, seeded into the rule store before `bind`, and rendered compactly on
    // the wire. The two rules above differ only in how the same body is written, so this also pins
    // that the object form is byte-identical to the escaped-string form it replaces.
    let resp = sut_client(intercept, &ca_pem)
        .get("https://cdn.example.com/datafiles/key-b.json")
        .send()
        .await
        .expect("an object serve body declared in the config file is intercepted");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), r#"{"featureX":"ON"}"#);

    // Issue #936: a numeric-string `statusCode` and a multi-value header declared in the config
    // file survive the same boot path. The config door is the one #933's note calls out as
    // ungated by `verify-docs-coverage.sh`, so it gets its own assertion rather than riding on
    // the admin-API tests.
    let resp = sut_client(intercept, &ca_pem)
        .get("https://cdn.example.com/datafiles/key-c.json")
        .send()
        .await
        .expect("a string statusCode and multi-value headers from the config file are intercepted");
    assert_eq!(resp.status(), 418, "the string \"418\" parsed as a status");
    let cookies: Vec<&str> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().expect("ascii cookie"))
        .collect();
    assert_eq!(
        cookies,
        vec!["a=1", "b=2"],
        "both values arrive as separate header lines"
    );
    assert_eq!(resp.text().await.unwrap(), r#"{"featureX":"ON"}"#);

    server.shutdown().await;
}

/// The driving use case end to end: one file declares the imposter *and* the rule that routes the
/// intercepted CDN call to it. Proves the two halves of the file are wired to each other at boot.
#[tokio::test]
async fn configfile_intercept_block_forwards_to_a_declared_imposter() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"{
            "imposters": [
                { "port": 24655, "protocol": "http", "name": "datafile",
                  "stubs": [{ "responses": [{ "is": { "statusCode": 200,
                                                      "body": "{\"flag\":\"from-imposter\"}" } }] }] }
            ],
            "intercept": {
                "port": 0,
                "rules": [{ "host": "cdn.example.com", "action": { "forward": { "port": 24655 } } }]
            }
        }"#,
    );

    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let intercept = server.intercept_addr().expect("listener bound");
    let ca_pem = reqwest::get(format!("http://{}/intercept/ca.pem", server.admin_addr()))
        .await
        .expect("ca.pem")
        .text()
        .await
        .unwrap();

    let resp = sut_client(intercept, &ca_pem)
        .get("https://cdn.example.com/datafiles/anything.json")
        .send()
        .await
        .expect("intercepted and forwarded to the imposter");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.unwrap(),
        r#"{"flag":"from-imposter"}"#,
        "the response must come from the imposter declared in the same file"
    );

    server.shutdown().await;
}

/// AC6 e2e: the seeded set is a starting point, not a closed set — runtime admin calls still layer.
#[tokio::test]
async fn runtime_admin_rules_layer_on_top_of_the_seeded_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"{
            "imposters": [],
            "intercept": {
                "port": 0,
                "rules": [{ "host": "seeded.example.com",
                            "action": { "serve": { "statusCode": 200, "body": "from-config" } } }]
            }
        }"#,
    );

    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("listener bound");
    let ca_pem = reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca.pem")
        .text()
        .await
        .unwrap();

    // The config-seeded rule is visible to the admin API — one store, two doors.
    let listed = reqwest::get(format!("http://{admin}/intercept/rules"))
        .await
        .expect("list rules")
        .text()
        .await
        .unwrap();
    assert!(
        listed.contains("seeded.example.com"),
        "the config-seeded rule must be listed by the admin API: {listed}"
    );

    let added = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(r#"{"host":"runtime.example.com","action":{"serve":{"statusCode":200,"body":"from-admin"}}}"#)
        .send()
        .await
        .expect("add a rule at runtime");
    assert_eq!(added.status(), 201);

    let client = sut_client(intercept, &ca_pem);
    let seeded = client
        .get("https://seeded.example.com/x")
        .send()
        .await
        .expect("seeded rule still matches");
    assert_eq!(seeded.text().await.unwrap(), "from-config");
    let runtime = client
        .get("https://runtime.example.com/x")
        .send()
        .await
        .expect("runtime rule matches too");
    assert_eq!(runtime.text().await.unwrap(), "from-admin");

    server.shutdown().await;
}

/// AC2: a config file with no block is byte-for-byte today's behaviour — no listener.
#[tokio::test]
async fn configfile_without_intercept_block_leaves_listener_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, r#"{"imposters":[{"port":24656,"protocol":"http"}]}"#);
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    assert!(
        server.intercept_addr().is_none(),
        "no block and no flag must leave the listener off"
    );
    server.shutdown().await;
}

/// AC3: two spellings of one listener is a startup error, not a silent precedence guess.
#[tokio::test]
async fn configfile_block_conflicts_with_intercept_port_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"{"imposters":[],"intercept":{"port":0,"rules":[]}}"#,
    );
    let msg = start_expecting_error(
        cli_with_config(&path, &["--intercept-port", "0"]),
        "an intercept block plus --intercept-port must abort startup",
    )
    .await;
    assert!(msg.contains("--intercept-port"), "names the flag: {msg}");
}

/// AC4: the config-file door gates injection for rules exactly as it does for imposters — an
/// `inject` predicate is executable code arriving by file.
#[tokio::test]
async fn configfile_intercept_rule_with_inject_is_refused_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"{
        "imposters": [],
        "intercept": { "port": 0, "rules": [
            { "host": "evil.example.com",
              "predicates": [{ "inject": "function (req) { return true; }" }],
              "action": { "serve": { "statusCode": 200 } } }
        ]}
    }"#;
    let path = write_config(&dir, body);

    let msg = start_expecting_error(
        cli_with_config(&path, &[]),
        "an inject predicate without --allowInjection must abort startup",
    )
    .await;
    assert!(
        msg.contains("--allowInjection"),
        "the error must name the flag that would allow it: {msg}"
    );

    // The flag is the whole point: with it set, the same file boots.
    let server = ServerBuilder::from_cli(cli_with_config(&path, &["--allow-injection"]))
        .start()
        .await
        .expect("--allowInjection admits the same config");
    assert!(server.intercept_addr().is_some());
    server.shutdown().await;
}

// ===== Issue #1271: `POST /admin/reload` re-applies the file's `intercept.rules` =====

async fn reload(admin: std::net::SocketAddr) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{admin}/admin/reload"))
        .send()
        .await
        .expect("reload");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

async fn rule_hosts(admin: std::net::SocketAddr) -> Vec<String> {
    let listed: serde_json::Value = reqwest::get(format!("http://{admin}/intercept/rules"))
        .await
        .expect("list rules")
        .json()
        .await
        .expect("rules json");
    listed
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["host"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn block_config(intercept: &str) -> String {
    format!(r#"{{ "imposters": [], "intercept": {intercept} }}"#)
}

fn serve_rule(host: &str, body: &str) -> String {
    format!(
        r#"{{ "host": "{host}", "action": {{ "serve": {{ "statusCode": 200, "body": "{body}" }} }} }}"#
    )
}

async fn ca_pem(admin: std::net::SocketAddr) -> String {
    reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca.pem")
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_reload_replaces_the_seeded_rules_and_keeps_runtime_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let client = sut_client(
        server.intercept_addr().expect("listener"),
        &ca_pem(admin).await,
    );

    let added = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(serve_rule("b.test", "runtime"))
        .send()
        .await
        .expect("add runtime rule");
    assert_eq!(added.status(), 201);

    write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}, {}] }}"#,
            serve_rule("a.test", "v2"),
            serve_rule("c.test", "new")
        )),
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["intercept"]["rulesSeeded"], 2, "{body}");
    assert_eq!(body["intercept"]["rulesRuntime"], 1, "{body}");
    assert!(
        body.get("warnings").is_none(),
        "nothing to warn about: {body}"
    );

    assert_eq!(
        rule_hosts(admin).await,
        vec!["a.test", "c.test", "b.test"],
        "the file's rules replace the seeded prefix; the runtime rule stays after them"
    );
    let served = client.get("https://a.test/x").send().await.expect("a.test");
    assert_eq!(served.text().await.unwrap(), "v2");
    let runtime = client.get("https://b.test/x").send().await.expect("b.test");
    assert_eq!(runtime.text().await.unwrap(), "runtime");
    server.shutdown().await;
}

/// After `DELETE /intercept/rules` there is no seeded prefix left; a reload puts the file's rules
/// back in front.
#[tokio::test]
async fn a_reload_after_clearing_the_rules_reseeds_the_file_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let http = reqwest::Client::new();
    http.delete(format!("http://{admin}/intercept/rules"))
        .send()
        .await
        .unwrap();
    http.post(format!("http://{admin}/intercept/rules"))
        .body(serve_rule("b.test", "runtime"))
        .send()
        .await
        .unwrap();

    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["intercept"]["rulesSeeded"], 1, "{body}");
    assert_eq!(rule_hosts(admin).await, vec!["a.test", "b.test"]);
    server.shutdown().await;
}

/// The listener is boot-only: a block whose listener fields changed keeps both the listener and
/// the rules, and the reply says which field it could not apply.
#[tokio::test]
async fn a_reload_whose_block_changes_the_listener_warns_and_keeps_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 1, "auth": {{ "username": "u", "password": "p" }}, "rules": [{}] }}"#,
            serve_rule("z.test", "v2")
        )),
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "imposters still reload: {body}");
    let warnings = body["warnings"].to_string();
    assert!(
        warnings.contains("port") && warnings.contains("auth"),
        "{body}"
    );
    assert!(!warnings.contains("host"), "host did not change: {body}");
    assert!(
        body.get("intercept").is_none(),
        "rules were not applied: {body}"
    );
    assert_eq!(rule_hosts(admin).await, vec!["a.test"]);
    server.shutdown().await;
}

#[tokio::test]
async fn a_flag_started_listener_ignores_a_new_block_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, r#"{ "imposters": [] }"#);
    let server = ServerBuilder::from_cli(cli_with_config(&path, &["--intercept-port", "0"]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    // No block in the file: nothing to say about the flag-started listener.
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("warnings").is_none() && body.get("intercept").is_none(),
        "{body}"
    );

    write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["warnings"].to_string().contains("--intercept"),
        "{body}"
    );
    assert!(body.get("intercept").is_none(), "{body}");
    assert!(rule_hosts(admin).await.is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn a_reload_without_a_running_listener_warns() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let stopped = reqwest::Client::new()
        .delete(format!("http://{admin}/intercept"))
        .send()
        .await
        .unwrap();
    assert_eq!(stopped.status(), 204);
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["warnings"]
            .to_string()
            .contains("no listener is running"),
        "{body}"
    );
    let status = reqwest::get(format!("http://{admin}/intercept"))
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404, "reload never starts a listener");
    server.shutdown().await;
}

/// All-or-nothing, like a gated imposter: a scripted rule refuses the reload before any imposter
/// changes.
#[tokio::test]
async fn a_reload_with_a_gated_inject_rule_is_refused_before_imposters_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    write_config(
        &dir,
        r#"{ "imposters": [ { "protocol": "http", "stubs": [] } ],
             "intercept": { "port": 0, "rules": [
               { "predicates": [{ "inject": "function (r) { return true; }" }],
                 "action": { "serve": { "statusCode": 200 } } } ] } }"#,
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("allowInjection"), "{body}");
    let imposters: serde_json::Value = reqwest::get(format!("http://{admin}/imposters"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        imposters["imposters"].as_array().map(Vec::len),
        Some(0),
        "{imposters}"
    );
    assert_eq!(rule_hosts(admin).await, vec!["a.test"]);
    server.shutdown().await;
}

/// Dropping the block from the file does not stop a listener the file started; the reply says so
/// rather than leaving the caller to guess, and the rules stay as they were.
#[tokio::test]
async fn a_reload_whose_file_drops_the_block_warns_and_keeps_the_listener() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    write_config(&dir, r#"{ "imposters": [] }"#);
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["warnings"].to_string().contains("no longer declares"),
        "{body}"
    );
    assert_eq!(rule_hosts(admin).await, vec!["a.test"]);
    assert!(server.intercept_addr().is_some());
    server.shutdown().await;
}

/// A listener started at runtime belongs to whoever started it; the file's block is not applied.
#[tokio::test]
async fn a_runtime_started_listener_ignores_a_new_block_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, r#"{ "imposters": [] }"#);
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let started = reqwest::Client::new()
        .post(format!("http://{admin}/intercept"))
        .body(format!(
            r#"{{ "rules": [{}] }}"#,
            serve_rule("runtime.test", "r")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 201);
    write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["warnings"].to_string().contains("started at runtime"),
        "{body}"
    );
    assert!(body.get("intercept").is_none(), "{body}");
    assert_eq!(rule_hosts(admin).await, vec!["runtime.test"]);
    server.shutdown().await;
}

/// The rules follow the imposters: a reload whose imposters fail to apply leaves them alone.
#[tokio::test]
async fn a_reload_that_fails_to_apply_imposters_leaves_the_rules_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    // An https imposter whose certificate does not parse fails at apply, after validation.
    write_config(
        &dir,
        &format!(
            r#"{{ "imposters": [ {{ "protocol": "https", "cert": "not a pem", "key": "not a pem",
                                   "stubs": [] }} ],
                 "intercept": {{ "port": 0, "rules": [{}] }} }}"#,
            serve_rule("z.test", "v2")
        ),
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 500, "the bad imposter fails the apply: {body}");
    assert!(body.get("intercept").is_none(), "{body}");
    assert_eq!(rule_hosts(admin).await, vec!["a.test"]);
    server.shutdown().await;
}

/// `--allowInjection` admits a scripted rule on reload as it does at boot.
#[tokio::test]
async fn a_reload_with_an_inject_rule_applies_under_allow_injection() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &["--allowInjection"]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    write_config(
        &dir,
        r#"{ "imposters": [], "intercept": { "port": 0, "rules": [
             { "host": "s.test", "predicates": [{ "inject": "function (r) { return true; }" }],
               "action": { "serve": { "statusCode": 200 } } } ] } }"#,
    );
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["intercept"]["rulesSeeded"], 1, "{body}");
    assert_eq!(rule_hosts(admin).await, vec!["s.test"]);
    server.shutdown().await;
}

/// `PUT /intercept/rules` makes every rule a runtime rule; a reload puts the file's back in front.
#[tokio::test]
async fn a_reload_after_put_puts_the_file_rules_in_front() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        &block_config(&format!(
            r#"{{ "port": 0, "rules": [{}] }}"#,
            serve_rule("a.test", "v1")
        )),
    );
    let server = ServerBuilder::from_cli(cli_with_config(&path, &[]))
        .start()
        .await
        .expect("server starts");
    let admin = server.admin_addr();
    let put = reqwest::Client::new()
        .put(format!("http://{admin}/intercept/rules"))
        .body(format!("[{}]", serve_rule("p.test", "p")))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    let (status, body) = reload(admin).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["intercept"]["rulesRuntime"], 1, "{body}");
    assert_eq!(rule_hosts(admin).await, vec!["a.test", "p.test"]);
    server.shutdown().await;
}
