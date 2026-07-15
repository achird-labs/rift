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
                                             "body": "{\"featureX\":\"ON\"}" } } }
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
