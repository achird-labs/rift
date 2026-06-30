//! Live integration tests for `rift-verify --verify-dynamic` (issue #251).
//!
//! Each test stands up an in-process `AdminApiServer` with dynamic imposters, then runs the
//! compiled `rift-verify` binary as a subprocess pointing at it. The binary stands up its own
//! embedded mock upstream, recreates throwaway imposters on free ports, and asserts the dynamic
//! behaviors end-to-end. Self-contained (no external server), so they run in the default suite.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-verify");

async fn start_admin(port: u16, manager: Arc<ImposterManager>) -> String {
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let server = rift_http_proxy::admin_api::AdminApiServer::new(addr, manager, None);
    tokio::spawn(server.run());
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    format!("http://127.0.0.1:{port}")
}

async fn create(manager: &ImposterManager, config: serde_json::Value) {
    let config = serde_json::from_value(config).expect("valid imposter config");
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
}

async fn run_verify(admin: &str) -> std::process::Output {
    Command::new(BIN)
        .args(["--admin-url", admin, "--skip-dynamic", "--verify-dynamic"])
        .output()
        .await
        .expect("run rift-verify")
}

#[tokio::test]
async fn verify_dynamic_asserts_proxy_verify_and_fault() {
    let manager = Arc::new(ImposterManager::new());

    // Mechanism 2: a repeat-cycling stub with a `_verify` sequence (clean state on a fresh imposter).
    create(
        &manager,
        serde_json::json!({
            "port": 19861, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/r" } }],
                "responses": [
                    { "is": { "statusCode": 503 }, "_behaviors": { "repeat": 2 } },
                    { "is": { "statusCode": 200 } }
                ],
                "_verify": { "sequence": [
                    { "request": { "path": "/r" }, "expect": { "status": 503 } },
                    { "request": { "path": "/r" }, "expect": { "status": 503 } },
                    { "request": { "path": "/r" }, "expect": { "status": 200 } },
                    { "request": { "path": "/r" }, "expect": { "status": 503 } }
                ]}
            }]
        }),
    )
    .await;

    // Mechanism 1: a proxyOnce stub with predicateGenerators (rift-verify repoints `to` at its mock).
    create(
        &manager,
        serde_json::json!({
            "port": 19862, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": {
                "to": "http://127.0.0.1:1", "mode": "proxyOnce",
                "predicateGenerators": [{ "matches": { "method": true, "path": true } }]
            }}]}]
        }),
    )
    .await;

    // Mechanism 3: a deterministic error fault.
    create(
        &manager,
        serde_json::json!({
            "port": 19863, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/err" } }],
                "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "fault": { "error": { "probability": 1.0, "status": 503, "body": "down" } } } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12671, manager).await;
    let out = run_verify(&admin).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "rift-verify should exit 0; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // AC6 _verify sequence, AC7 proxy sentinel + record, AC8 fault status.
    assert!(
        stdout.contains("_verify["),
        "missing _verify checks:\n{stdout}"
    );
    assert!(
        stdout.contains("proxied sentinel"),
        "missing proxy check:\n{stdout}"
    );
    assert!(
        stdout.contains("records stub (+1)"),
        "missing proxy record check:\n{stdout}"
    );
    assert!(
        stdout.contains("error status 503"),
        "missing fault check:\n{stdout}"
    );
    assert!(
        !stdout.contains("FAIL"),
        "no dynamic check should fail:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_dynamic_fails_on_wrong_expectation() {
    let manager = Arc::new(ImposterManager::new());
    // A `_verify` step asserts the wrong status — the verifier must FAIL (exit 1).
    create(
        &manager,
        serde_json::json!({
            "port": 19871, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/ok" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "ok" } }],
                "_verify": { "sequence": [
                    { "request": { "path": "/ok" }, "expect": { "status": 418 } }
                ]}
            }]
        }),
    )
    .await;

    let admin = start_admin(12681, manager).await;
    let out = run_verify(&admin).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "should exit non-zero on a failed expectation:\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "should report a FAIL:\n{stdout}");
}

#[tokio::test]
async fn dynamic_skipped_without_flag() {
    // The opt-in contract: WITHOUT --verify-dynamic, dynamic stubs are skipped (no assertion,
    // no mock upstream, no throwaway imposters) — the original safe behavior is preserved.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19881, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": {
                "to": "http://127.0.0.1:1", "mode": "proxyOnce",
                "predicateGenerators": [{ "matches": { "path": true } }]
            }}]}]
        }),
    )
    .await;

    let admin = start_admin(12691, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin, "--skip-dynamic"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "should exit 0:\n{stdout}");
    assert!(
        !stdout.contains("Dynamic assertions"),
        "no dynamic section should appear without --verify-dynamic:\n{stdout}"
    );
    assert!(
        !stdout.contains("proxied sentinel"),
        "no proxy assertion should run without the flag:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_dynamic_asserts_tcp_fault() {
    // Issue #258: a real `_rift.fault.tcp` reset surfaces as a reqwest error whose top-level
    // Display is only "error sending request for url (...)"; the verifier must classify it as a
    // transport reset (via the source chain) and report PASS, not FAIL.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19891, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/reset" } }],
                "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12701, manager).await;
    let out = run_verify(&admin).await;
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "tcp-fault assertion should PASS; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("tcp reset"),
        "missing tcp reset check:\n{stdout}"
    );
    assert!(
        !stdout.contains("FAIL"),
        "tcp reset must not FAIL:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_normal_pass_asserts_tcp_fault() {
    // Issue #258: the NORMAL verification pass (no --verify-dynamic) also classifies a real
    // `_rift.fault.tcp` reset via execute_test's Err arm — guards that fix site against regression.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19901, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/reset" } }],
                "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12711, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin]) // normal mode: no --skip-dynamic, no --verify-dynamic
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "normal-pass tcp-fault must PASS (reset is the expected outcome); stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("FAIL"),
        "tcp reset must not FAIL in the normal pass:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_normal_pass_accepts_date_template_body() {
    // Issue #259: a stub whose is.body carries Rift date templates ({{NOW}}/{{DAYS+N}}) is
    // expanded by the engine; the verifier must not assert the literal template body and FAIL.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19911, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/token" } }],
                "responses": [{ "is": { "statusCode": 200,
                    "body": { "issued": "{{NOW}}", "expires": "{{DAYS+30}}", "kind": "token" } } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12721, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin]) // normal verification pass
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "date-template body must not FAIL; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("FAIL"),
        "no body-mismatch FAIL for template body:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_normal_pass_accepts_plaintext_template_body() {
    // Issue #259: a NON-JSON (plain-text) body carrying a template expands to a bare string that
    // is not valid JSON, so it exercises verify_response's plain-text-compare branch — which must
    // also skip the literal assertion rather than report a body mismatch.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19921, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/snapshot" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "snapshot taken at {{NOW}}" } }]
            }]
        }),
    )
    .await;

    let admin = start_admin(12731, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "plain-text template body must not FAIL; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("FAIL"),
        "no mismatch FAIL for plain-text template body:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_normal_pass_drives_space_partitioned_stubs() {
    // Issue #260: a correlated-isolation imposter with two stubs partitioned by `space` on the
    // same path. The verifier must drive each stub with its OWN space value.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19931, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory",
                "flowIdSource": "header:X-Mock-Space" } },
            "stubs": [
                { "space": "alice", "predicates": [{ "equals": { "path": "/data" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": { "owner": "alice" } } }] },
                { "space": "bob", "predicates": [{ "equals": { "path": "/data" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": { "owner": "bob" } } }] }
            ]
        }),
    )
    .await;

    let admin = start_admin(12741, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "space-partitioned stubs must each PASS; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("FAIL"),
        "no space-mismatch FAIL:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_flow_id_header_does_not_clobber_detection() {
    // Issue #260: a detected per-imposter header takes precedence over --flow-id-header, so a
    // (here deliberately wrong) global override does NOT clobber a correctly-detected imposter.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19951, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory",
                "flowIdSource": "header:X-Mock-Space" } },
            "stubs": [
                { "space": "alice", "predicates": [{ "equals": { "path": "/d" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": { "o": "alice" } } }] },
                { "space": "bob", "predicates": [{ "equals": { "path": "/d" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": { "o": "bob" } } }] }
            ]
        }),
    )
    .await;

    let admin = start_admin(12761, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin, "--flow-id-header", "X-Wrong-Header"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Detection (X-Mock-Space) wins over the wrong override, so both stubs still PASS.
    assert!(
        out.status.success(),
        "detection must win over a wrong --flow-id-header; stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("FAIL"),
        "no FAIL when detection wins:\n{stdout}"
    );
}

#[tokio::test]
async fn verify_skips_space_stub_when_flow_id_unresolvable() {
    // Issue #260 (silent-failure guard): a stub declares `space` but the imposter exposes no
    // flowIdSource and no --flow-id-header is given → the verifier must SKIP it with a reason,
    // not silently run a degraded (mis-routed) check.
    let manager = Arc::new(ImposterManager::new());
    create(
        &manager,
        serde_json::json!({
            "port": 19961, "protocol": "http",
            "stubs": [{ "space": "alice", "predicates": [{ "equals": { "path": "/d" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "A" } }] }]
        }),
    )
    .await;

    let admin = start_admin(12771, manager).await;
    let out = Command::new(BIN)
        .args(["--admin-url", &admin, "--verbose"])
        .output()
        .await
        .expect("run rift-verify");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "an unverifiable space stub is a skip, not a failure:\n{stdout}"
    );
    assert!(
        stdout.contains("flowIdSource"),
        "the skip reason must explain the unresolved flowIdSource:\n{stdout}"
    );
}
