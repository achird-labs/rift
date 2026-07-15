//! `POST /intercept/rules` obeys `--allowInjection` (issue #657).
//!
//! An intercept rule's predicates are evaluated per intercepted request, so an `inject` predicate
//! is executable code admitted over the admin API — which is unauthenticated unless `--apikey` is
//! set. Before this fix the identical predicate was refused by `POST /imposters` and executed here.
//!
//! These drive the real server end to end, because the gate's value is that the JS never runs: a
//! unit test on the handler proves the 400, only this proves nothing executed.

use clap::Parser;
use rift_http_proxy::server::{Cli, ServerBuilder};

/// The rule from the issue's probe: the script returns true only for `/pwned`, so a match proves
/// the JS was evaluated per-request.
const INJECT_RULE: &str = r#"{"host":"probe.example.com",
    "predicates":[{"inject":"function (request) { return request.path === '/pwned'; }"}],
    "action":{"serve":{"statusCode":200,"body":"INJECT-EXECUTED"}}}"#;

fn cli(extra: &[&str]) -> Cli {
    let mut args = vec![
        "rift",
        "--local-only",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--intercept-port",
        "0",
    ];
    args.extend_from_slice(extra);
    Cli::parse_from(args)
}

/// Without the flag: refused with the same message every other door gives, nothing stored, and —
/// the point — the script never runs.
#[tokio::test]
async fn intercept_rules_inject_is_refused_and_never_executes_without_the_flag() {
    let server = ServerBuilder::from_cli(cli(&[]))
        .start()
        .await
        .expect("start");
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("listener");

    let resp = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(INJECT_RULE)
        .send()
        .await
        .expect("post rule");
    assert_eq!(
        resp.status(),
        400,
        "an inject predicate must be refused without --allowInjection"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("allowInjection"),
        "the refusal must name the flag that would allow it: {body}"
    );

    let listed = reqwest::get(format!("http://{admin}/intercept/rules"))
        .await
        .expect("list")
        .text()
        .await
        .unwrap();
    assert_eq!(
        listed.trim(),
        "[]",
        "a refused rule must not be stored: {listed}"
    );

    // The whole point: the script does not run. Without the fix this returned "INJECT-EXECUTED".
    let ca_pem = reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca")
        .text()
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{intercept}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let served = client
        .get("https://probe.example.com/pwned")
        .send()
        .await
        .expect("request");
    let served_body = served.text().await.unwrap_or_default();
    assert!(
        !served_body.contains("INJECT-EXECUTED"),
        "the refused rule's script must never execute, got: {served_body}"
    );

    server.shutdown().await;
}

/// With the flag: admitted and executed — the `/pwned` vs `/other` differential proves the engine
/// really evaluates the predicate, so the test above is measuring a gate and not a broken engine.
#[tokio::test]
async fn intercept_rules_inject_executes_with_allow_injection() {
    let server = ServerBuilder::from_cli(cli(&["--allow-injection"]))
        .start()
        .await
        .expect("start");
    let admin = server.admin_addr();
    let intercept = server.intercept_addr().expect("listener");

    let resp = reqwest::Client::new()
        .post(format!("http://{admin}/intercept/rules"))
        .body(INJECT_RULE)
        .send()
        .await
        .expect("post rule");
    assert_eq!(resp.status(), 201, "--allowInjection must admit the rule");

    let ca_pem = reqwest::get(format!("http://{admin}/intercept/ca.pem"))
        .await
        .expect("ca")
        .text()
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{intercept}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();

    let hit = client
        .get("https://probe.example.com/pwned")
        .send()
        .await
        .expect("request");
    assert_eq!(hit.text().await.unwrap(), "INJECT-EXECUTED");

    let miss = client
        .get("https://probe.example.com/other")
        .send()
        .await
        .expect("request");
    assert!(
        !miss
            .text()
            .await
            .unwrap_or_default()
            .contains("INJECT-EXECUTED"),
        "the script returns false for /other — proving it is evaluated per request"
    );

    server.shutdown().await;
}
