//! Issue #356: admin-API-created imposters resolve `_rift.script.file:` under `--scripts-dir`,
//! rejecting any path that escapes the root (never reading it), and rejecting `file:` outright
//! when no `--scripts-dir` was configured.

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;

async fn wait_for_http(url: &str) {
    for _ in 0..50 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("server at {url} did not come up");
}

fn config_with_file_script(port: u16, file: &str) -> serde_json::Value {
    serde_json::json!({
        "port": port,
        "protocol": "http",
        "stubs": [{
            "responses": [{ "_rift": { "script": { "file": file } } }]
        }]
    })
}

#[tokio::test]
async fn file_script_within_scripts_dir_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("should_inject.rhai"),
        r#"fn should_inject(request, flow_store) { #{ inject: false } }"#,
    )
    .unwrap();

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .with_scripts_dir(dir.path().to_path_buf())
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/imposters"))
        .json(&config_with_file_script(19551, "should_inject.rhai"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 201, "in-root file: must be accepted");

    running.shutdown().await;
}

#[tokio::test]
async fn file_script_escaping_scripts_dir_is_rejected_without_reading() {
    let root_parent = tempfile::tempdir().expect("tempdir");
    // A real, readable file OUTSIDE the scripts root — proves rejection is the `..` escape
    // check, not merely a missing file.
    std::fs::write(root_parent.path().join("secret.rhai"), "SECRET").unwrap();
    let scripts_dir = root_parent.path().join("scripts");
    std::fs::create_dir(&scripts_dir).unwrap();

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .with_scripts_dir(scripts_dir)
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/imposters"))
        .json(&config_with_file_script(19552, "../secret.rhai"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        400,
        "a file: path escaping --scripts-dir must be rejected"
    );
    let body: serde_json::Value = resp.json().await.expect("json body");
    let message = body["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("escapes"),
        "error should explain the escape, got: {message}"
    );

    running.shutdown().await;
}

#[tokio::test]
async fn file_script_without_scripts_dir_configured_is_rejected() {
    let manager = Arc::new(ImposterManager::new());
    // No `.with_scripts_dir(...)` — admin-API file: references have nowhere to resolve.
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/imposters"))
        .json(&config_with_file_script(19553, "should_inject.rhai"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);

    running.shutdown().await;
}

#[tokio::test]
async fn ref_to_unknown_registry_entry_is_rejected() {
    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let config = serde_json::json!({
        "port": 19554,
        "protocol": "http",
        "stubs": [{
            "responses": [{ "_rift": { "script": { "ref": "doesNotExist" } } }]
        }]
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/imposters"))
        .json(&config)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);

    running.shutdown().await;
}

#[tokio::test]
async fn named_registry_ref_resolves_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("fail-twice.rhai"),
        r#"fn should_inject(request, flow_store) { #{ inject: true, fault: "error", status: 503, body: "nope" } }"#,
    )
    .unwrap();

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .with_scripts_dir(dir.path().to_path_buf())
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let config = serde_json::json!({
        "port": 19555,
        "protocol": "http",
        "_rift": { "scripts": { "failTwice": { "file": "fail-twice.rhai" } } },
        "stubs": [{
            "responses": [{ "_rift": { "script": { "ref": "failTwice" } } }]
        }]
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/imposters"))
        .json(&config)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        201,
        "ref: to a file-backed registry entry must resolve"
    );

    let imposter_resp = client
        .get("http://127.0.0.1:19555/anything")
        .send()
        .await
        .expect("hit the imposter");
    assert_eq!(
        imposter_resp.status(),
        503,
        "the resolved script must actually run"
    );

    running.shutdown().await;
}

// Issue #356 B1: the stub sub-resource endpoints resolve & escape-check `file:` at WRITE time,
// exactly like whole-imposter create — so a network-authored `file:` escape is a 400 (never
// persisted), and an in-root file resolves and actually runs at request time (no unresolved 500).
#[tokio::test]
async fn stub_add_endpoint_rejects_escaping_file_and_resolves_in_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("secret.rhai"), "SECRET").unwrap();
    let root = dir.path().join("scripts");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("boom.rhai"),
        r#"fn should_inject(request, flow_store) { #{ inject: true, fault: "error", status: 503, body: "boom" } }"#,
    )
    .unwrap();

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .with_scripts_dir(root)
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let client = reqwest::Client::new();
    // Create the target imposter (no stubs yet).
    let create = client
        .post(format!("http://{addr}/imposters"))
        .json(&serde_json::json!({ "port": 19556, "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("create");
    assert_eq!(create.status(), 201);

    // POST a stub whose `file:` escapes the scripts root → 400, and the escaping stub is NOT
    // persisted (the imposter still has zero stubs).
    let escaping = client
        .post(format!("http://{addr}/imposters/19556/stubs"))
        .json(&serde_json::json!({
            "stub": { "responses": [{ "_rift": { "script": { "file": "../secret.rhai" } } }] }
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        escaping.status(),
        400,
        "escaping file: via stub add must 400"
    );
    let stubs_now: serde_json::Value = client
        .get(format!("http://{addr}/imposters/19556/stubs"))
        .send()
        .await
        .expect("get stubs")
        .json()
        .await
        .expect("json");
    assert_eq!(
        stubs_now["stubs"].as_array().map(|a| a.len()),
        Some(0),
        "the rejected stub must not have been persisted"
    );

    // POST a stub with an in-root `file:` → accepted, resolved, and it actually runs.
    let ok = client
        .post(format!("http://{addr}/imposters/19556/stubs"))
        .json(&serde_json::json!({
            "stub": { "responses": [{ "_rift": { "script": { "file": "boom.rhai" } } }] }
        }))
        .send()
        .await
        .expect("send");
    assert!(
        ok.status().is_success(),
        "in-root file: via stub add must be accepted, got {}",
        ok.status()
    );
    let served = client
        .get("http://127.0.0.1:19556/anything")
        .send()
        .await
        .expect("hit imposter");
    assert_eq!(
        served.status(),
        503,
        "the file-resolved script must run (no unresolved 500)"
    );

    running.shutdown().await;
}

// Issue #356 B1: PUT .../stubs (replace-all) is escape-checked too.
#[tokio::test]
async fn stub_replace_all_endpoint_rejects_escaping_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("scripts");
    std::fs::create_dir(&root).unwrap();

    let manager = Arc::new(ImposterManager::new());
    let running = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager, None)
        .with_allow_injection(true)
        .with_scripts_dir(root)
        .bind()
        .await
        .expect("bind admin");
    let addr = running.local_addr();
    wait_for_http(&format!("http://{addr}/health")).await;

    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/imposters"))
        .json(&serde_json::json!({ "port": 19557, "protocol": "http", "stubs": [] }))
        .send()
        .await
        .expect("create");

    let resp = client
        .put(format!("http://{addr}/imposters/19557/stubs"))
        .json(&serde_json::json!({
            "stubs": [{ "responses": [{ "_rift": { "script": { "file": "/etc/passwd" } } }] }]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        400,
        "absolute file: via replace-all must 400"
    );

    running.shutdown().await;
}

// Issue #356: if an UNRESOLVED script (a `file:` never resolved into `code`) somehow reaches the
// serve path — a defense-in-depth invariant, since every write path now resolves — the handler
// must return a clear 500 with `x-rift-script-error`, not silently run an empty script. Built by
// creating the imposter directly through the manager (bypassing the resolving admin handlers).
#[tokio::test]
async fn unresolved_script_at_serve_time_returns_500() {
    // `file` set, `code` absent — the shape a resolve pass would have collapsed but here never ran.
    let config = serde_json::from_value(serde_json::json!({
        "port": 19558,
        "protocol": "http",
        "stubs": [{ "responses": [{ "_rift": { "script": { "file": "unresolved.rhai" } } }] }]
    }))
    .expect("deserialize config");

    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(config)
        .await
        .expect("create imposter directly (no resolve pass)");

    let resp = reqwest::get("http://127.0.0.1:19558/anything")
        .await
        .expect("hit imposter");
    assert_eq!(
        resp.status(),
        500,
        "an unresolved file:/ref: script must be a 500, not a silent empty run"
    );
    assert_eq!(
        resp.headers()
            .get("x-rift-script-error")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "the 500 must carry x-rift-script-error"
    );

    manager.delete_all().await;
}
