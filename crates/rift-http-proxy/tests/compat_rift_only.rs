//! Rift-only behaviour ported from the retired `tests/compatibility` cucumber suite (`@rift-only`
//! scenarios that had no other coverage): `X-Rift-Debug`, `pathRewrite`, stub listing, script
//! validation over HTTP, `_rift` fault headers / empty `_rift`, inject header casing, and
//! `_rift.warnings`.
//!
//! Each test spawns its own server on a private port slot (admin, imposter, backend) taken from
//! the block reserved for this file in `test_port_uniqueness.rs`.

mod support;

use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio::time::sleep;

const HOST: &str = "http://127.0.0.1";
const PORT_BASE: u16 = 20100;
const SLOT: u16 = 3;
/// Last port of this file's block in `test_port_uniqueness.rs`'s `RESERVED` list.
const PORT_END: u16 = 20199;

static NEXT_SLOT: AtomicU16 = AtomicU16::new(PORT_BASE);

/// A spawned Rift server, killed on drop. `imposter` and `backend` are free ports for the test.
struct Rift {
    admin: u16,
    imposter: u16,
    backend: u16,
    client: Client,
    _child: tokio::process::Child,
}

impl Rift {
    async fn start() -> Self {
        let admin = NEXT_SLOT.fetch_add(SLOT, Ordering::SeqCst);
        assert!(
            admin + SLOT - 1 <= PORT_END,
            "compat_rift_only.rs has outgrown its reserved ports {PORT_BASE}-{PORT_END}: widen the \
             block in test_port_uniqueness.rs"
        );
        let child = tokio::process::Command::new(support::server_bin())
            .args(["--port", &admin.to_string(), "--allow-injection"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn rift server");
        let client = Client::builder().no_proxy().build().expect("http client");
        for _ in 0..100 {
            if client
                .get(format!("{HOST}:{admin}/"))
                .timeout(Duration::from_millis(200))
                .send()
                .await
                .is_ok()
            {
                return Self {
                    admin,
                    imposter: admin + 1,
                    backend: admin + 2,
                    client,
                    _child: child,
                };
            }
            sleep(Duration::from_millis(100)).await;
        }
        panic!("rift server did not start within 10s");
    }

    fn admin_url(&self, path: &str) -> String {
        format!("{HOST}:{}{path}", self.admin)
    }

    fn imposter_url(&self, port: u16, path: &str) -> String {
        format!("{HOST}:{port}{path}")
    }

    /// POST /imposters and return the raw response (callers assert on status themselves).
    async fn post_imposter(&self, config: &Value) -> Response {
        self.client
            .post(self.admin_url("/imposters"))
            .json(config)
            .send()
            .await
            .expect("POST /imposters")
    }

    /// POST /imposters, asserting 201.
    async fn create(&self, config: &Value) -> Value {
        let resp = self.post_imposter(config).await;
        assert_eq!(resp.status().as_u16(), 201, "create imposter");
        resp.json().await.expect("create response is JSON")
    }

    /// GET with an optional header, returning (status, body text).
    async fn get(&self, url: &str, header: Option<(&str, &str)>) -> (u16, String) {
        let mut req = self.client.get(url);
        if let Some((k, v)) = header {
            req = req.header(k, v);
        }
        let resp = req.send().await.expect("GET");
        let status = resp.status().as_u16();
        (status, resp.text().await.expect("body text"))
    }

    /// Send a debug-mode request and parse the JSON report.
    async fn debug(&self, path: &str, header: (&str, &str)) -> Value {
        let (status, body) = self
            .get(&self.imposter_url(self.imposter, path), Some(header))
            .await;
        assert_eq!(status, 200, "debug response status");
        serde_json::from_str(&body).expect("debug response is JSON")
    }
}

// =============================================================================
// debug_mode.feature
// =============================================================================

/// Ported from tests/compatibility/features/debug_mode.feature: "Debug mode returns match
/// information for matching stub"
#[tokio::test]
async fn debug_mode_returns_match_information_for_matching_stub() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http", "name": "Test Service",
        "stubs": [{
            "id": "get-users",
            "predicates": [{"equals": {"method": "GET", "path": "/api/users"}}],
            "responses": [{"is": {"statusCode": 200, "body": "users list"}}]
        }]
    }))
    .await;
    let v = rift.debug("/api/users", ("X-Rift-Debug", "true")).await;
    assert_eq!(v["debug"], true);
    assert_eq!(v["matchResult"]["matched"], true);
    assert_eq!(v["matchResult"]["stubIndex"], 0);
    assert_eq!(v["matchResult"]["stubId"], "get-users");
}

/// Ported from debug_mode.feature: "Debug mode shows response preview"
#[tokio::test]
async fn debug_mode_shows_response_preview() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"path": "/test"}}],
            "responses": [{"is": {"statusCode": 201, "body": "created"}}]
        }]
    }))
    .await;
    let v = rift.debug("/test", ("X-Rift-Debug", "true")).await;
    let preview = &v["matchResult"]["responsePreview"];
    assert_eq!(preview["statusCode"], 201);
    assert_eq!(preview["bodyPreview"], "created");
}

/// Ported from debug_mode.feature: "Debug mode shows all stubs when no match found"
#[tokio::test]
async fn debug_mode_shows_all_stubs_when_no_match() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [
            {"id": "stub-a", "predicates": [{"equals": {"path": "/a"}}], "responses": [{"is": {"body": "A"}}]},
            {"id": "stub-b", "predicates": [{"equals": {"path": "/b"}}], "responses": [{"is": {"body": "B"}}]}
        ]
    }))
    .await;
    let v = rift.debug("/not-found", ("X-Rift-Debug", "true")).await;
    let m = &v["matchResult"];
    assert_eq!(m["matched"], false);
    assert_eq!(m["reason"], "No stub predicates matched the request");
    let all = m["allStubs"].as_array().expect("allStubs is an array");
    assert_eq!(all.len(), 2);
    let ids: Vec<&str> = all.iter().filter_map(|s| s["id"].as_str()).collect();
    assert_eq!(ids, ["stub-a", "stub-b"]);
}

/// Ported from debug_mode.feature: "Debug mode shows reason when no stubs configured"
#[tokio::test]
async fn debug_mode_shows_reason_when_no_stubs_configured() {
    let rift = Rift::start().await;
    rift.create(&json!({"port": rift.imposter, "protocol": "http", "stubs": []}))
        .await;
    let v = rift.debug("/any", ("X-Rift-Debug", "true")).await;
    assert_eq!(v["matchResult"]["matched"], false);
    assert_eq!(
        v["matchResult"]["reason"],
        "No stubs configured for this imposter"
    );
}

/// Ported from debug_mode.feature: "Debug mode does not execute the actual response"
#[tokio::test]
async fn debug_mode_does_not_execute_the_actual_response() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http", "recordRequests": true,
        "stubs": [{
            "predicates": [{"equals": {"path": "/test"}}],
            "responses": [{"is": {"statusCode": 418, "body": "I'm a teapot"}}]
        }]
    }))
    .await;
    let (status, body) = rift
        .get(
            &rift.imposter_url(rift.imposter, "/test"),
            Some(("X-Rift-Debug", "true")),
        )
        .await;
    assert_eq!(status, 200);
    // The old scenario asserted the body text appears nowhere; the report's `bodyPreview` now
    // legitimately quotes it, so assert the body is the JSON report rather than the raw response.
    assert_ne!(body, "I'm a teapot");
    let v: Value = serde_json::from_str(&body).expect("debug body is JSON");
    assert_eq!(v["debug"], true);
    assert_eq!(
        v["matchResult"]["responsePreview"]["bodyPreview"],
        "I'm a teapot"
    );
    assert_eq!(v["matchResult"]["responsePreview"]["statusCode"], 418);
}

/// Ported from debug_mode.feature: "Debug mode accepts lowercase header"
#[tokio::test]
async fn debug_mode_accepts_lowercase_header() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"predicates": [], "responses": [{"is": {"body": "catch-all"}}]}]
    }))
    .await;
    let v = rift.debug("/any", ("x-rift-debug", "true")).await;
    assert_eq!(v["debug"], true);
}

/// Ported from debug_mode.feature: "Debug mode accepts value \"1\""
#[tokio::test]
async fn debug_mode_accepts_value_one() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"predicates": [], "responses": [{"is": {"body": "catch-all"}}]}]
    }))
    .await;
    let v = rift.debug("/any", ("X-Rift-Debug", "1")).await;
    assert_eq!(v["debug"], true);
}

/// Ported from debug_mode.feature: "Debug mode shows request details"
#[tokio::test]
async fn debug_mode_shows_request_details() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"predicates": [], "responses": [{"is": {"body": "catch-all"}}]}]
    }))
    .await;
    let v = rift
        .debug("/test/path?foo=bar", ("X-Rift-Debug", "true"))
        .await;
    assert_eq!(v["request"]["method"], "GET");
    assert_eq!(v["request"]["path"], "/test/path");
    assert_eq!(v["request"]["query"], "foo=bar");
}

/// Ported from debug_mode.feature: "Debug mode shows imposter information"
#[tokio::test]
async fn debug_mode_shows_imposter_information() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http", "name": "My Test Service",
        "stubs": [{"predicates": [], "responses": [{"is": {"body": "ok"}}]}]
    }))
    .await;
    let v = rift.debug("/any", ("X-Rift-Debug", "true")).await;
    assert_eq!(v["imposter"]["port"], rift.imposter);
    assert_eq!(v["imposter"]["name"], "My Test Service");
    assert_eq!(v["imposter"]["stubCount"], 1);
}

// =============================================================================
// proxy.feature: pathRewrite
// =============================================================================

/// Create a backend imposter that answers `path:<request path>` and a proxyAlways imposter with the
/// given `pathRewrite`, then request `request_path` through the proxy; returns (status, body).
async fn proxy_with_path_rewrite(from: &str, to: &str, request_path: &str) -> (u16, String) {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.backend, "protocol": "http", "recordRequests": true,
        "stubs": [{"predicates": [], "responses": [{
            "inject": "function(config) { return { statusCode: 200, body: 'path:' + config.request.path }; }"
        }]}]
    }))
    .await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"responses": [{"proxy": {
            "to": format!("http://localhost:{}", rift.backend),
            "mode": "proxyAlways",
            "pathRewrite": {"from": from, "to": to}
        }}]}]
    }))
    .await;
    rift.get(&rift.imposter_url(rift.imposter, request_path), None)
        .await
}

/// Ported from tests/compatibility/features/proxy.feature: "pathRewrite removes path prefix when
/// proxying"
#[tokio::test]
async fn path_rewrite_removes_path_prefix() {
    let (status, body) = proxy_with_path_rewrite("/api/v1", "", "/api/v1/users").await;
    assert_eq!(status, 200);
    assert_eq!(body, "path:/users");
}

/// Ported from proxy.feature: "pathRewrite replaces path prefix"
#[tokio::test]
async fn path_rewrite_replaces_path_prefix() {
    let (status, body) = proxy_with_path_rewrite("/old-api", "/new-api", "/old-api/resource").await;
    assert_eq!(status, 200);
    assert_eq!(body, "path:/new-api/resource");
}

/// Ported from proxy.feature: "pathRewrite does not modify non-matching paths"
#[tokio::test]
async fn path_rewrite_leaves_non_matching_path_unchanged() {
    let (status, body) = proxy_with_path_rewrite("/api/v1", "", "/other/path").await;
    assert_eq!(status, 200);
    assert_eq!(body, "path:/other/path");
}

// =============================================================================
// admin_api.feature: stub listing
// =============================================================================

/// Ported from tests/compatibility/features/admin_api.feature: "Get all stubs for an imposter"
#[tokio::test]
async fn get_all_stubs_for_an_imposter() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [
            {"predicates": [{"equals": {"path": "/first"}}], "responses": [{"is": {"statusCode": 200, "body": "first"}}]},
            {"predicates": [{"equals": {"path": "/second"}}], "responses": [{"is": {"statusCode": 200, "body": "second"}}]}
        ]
    }))
    .await;
    let resp = rift
        .client
        .get(rift.admin_url(&format!("/imposters/{}/stubs", rift.imposter)))
        .send()
        .await
        .expect("GET stubs");
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.expect("stubs JSON");
    let stubs = v["stubs"].as_array().expect("stubs array");
    assert_eq!(stubs.len(), 2);
    assert_eq!(stubs[0]["predicates"][0]["equals"]["path"], "/first");
    assert_eq!(stubs[1]["predicates"][0]["equals"]["path"], "/second");
}

/// Ported from admin_api.feature: "Get stub by index"
#[tokio::test]
async fn get_stub_by_index() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [
            {"predicates": [{"equals": {"path": "/zero"}}], "responses": [{"is": {"statusCode": 200, "body": "stub zero"}}]},
            {"predicates": [{"equals": {"path": "/one"}}], "responses": [{"is": {"statusCode": 200, "body": "stub one"}}]}
        ]
    }))
    .await;
    for (index, path, body) in [(0, "/zero", "stub zero"), (1, "/one", "stub one")] {
        let resp = rift
            .client
            .get(rift.admin_url(&format!("/imposters/{}/stubs/{index}", rift.imposter)))
            .send()
            .await
            .expect("GET stub");
        assert_eq!(resp.status().as_u16(), 200, "stub {index}");
        let v: Value = resp.json().await.expect("stub JSON");
        assert_eq!(v["predicates"][0]["equals"]["path"], path);
        assert_eq!(v["responses"][0]["is"]["body"], body);
    }
}

/// Ported from tests/compatibility/features/admin_api.feature: "Get stubs for non-existent imposter
/// returns 404", "Get stub for non-existent imposter returns 404" and "Get stub with out-of-range
/// index returns 404". Mountebank 2.9.1 has no `GET .../stubs` route, so these are Rift-only and
/// left the differential harness.
#[tokio::test]
async fn stub_reads_answer_404_for_a_missing_imposter_or_index() {
    let rift = Rift::start().await;
    for path in ["stubs", "stubs/0"] {
        let resp = rift
            .client
            .get(rift.admin_url(&format!("/imposters/{}/{path}", rift.imposter)))
            .send()
            .await
            .expect("GET stubs of a missing imposter");
        assert_eq!(resp.status().as_u16(), 404, "{path}");
        let v: Value = resp.json().await.expect("error JSON");
        assert_eq!(v["errors"][0]["type"], "no such resource", "{path}");
    }

    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]
    }))
    .await;
    let resp = rift
        .client
        .get(rift.admin_url(&format!("/imposters/{}/stubs/99", rift.imposter)))
        .send()
        .await
        .expect("GET out-of-range stub");
    assert_eq!(resp.status().as_u16(), 404);
    let v: Value = resp.json().await.expect("error JSON");
    assert_eq!(v["errors"][0]["message"], "Stub index 99 not found");
}

// =============================================================================
// rift_extensions.feature: script validation
// =============================================================================

async fn assert_create_rejected(rift: &Rift, config: &Value, expected: &[&str]) {
    let resp = rift.post_imposter(config).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.expect("error body");
    for needle in expected {
        assert!(body.contains(needle), "{needle:?} not in {body}");
    }
}

fn script_imposter(port: u16, engine: &str, code: &str) -> Value {
    json!({
        "port": port, "protocol": "http",
        "stubs": [{"responses": [{"_rift": {"script": {"engine": engine, "code": code}}}]}]
    })
}

/// Ported from tests/compatibility/features/rift_extensions.feature: "Rhai script with syntax
/// error is rejected at creation time"
#[tokio::test]
async fn rhai_syntax_error_is_rejected_with_400() {
    let rift = Rift::start().await;
    let cfg = script_imposter(
        rift.imposter,
        "rhai",
        "fn should_inject(request, flow_store) { #{ inject: ",
    );
    assert_create_rejected(&rift, &cfg, &["Script validation failed"]).await;
}

/// Ported from rift_extensions.feature: "Lua script with syntax error is rejected at creation
/// time". Lua was removed (#450), so today `engine: lua` is rejected as an unknown engine.
#[tokio::test]
async fn engine_lua_is_rejected_with_400() {
    let rift = Rift::start().await;
    let cfg = script_imposter(
        rift.imposter,
        "lua",
        "function should_inject(request, flow_store) return { inject =",
    );
    assert_create_rejected(&rift, &cfg, &["Script validation failed"]).await;
}

/// Ported from rift_extensions.feature: "Rhai script missing should_inject function is
/// rejected". `should_inject` became optional (#453), so today this is accepted.
#[tokio::test]
async fn rhai_without_should_inject_is_accepted() {
    let rift = Rift::start().await;
    let cfg = script_imposter(rift.imposter, "rhai", "fn wrong_function_name(x) { x + 1 }");
    let resp = rift.post_imposter(&cfg).await;
    assert_eq!(resp.status().as_u16(), 201);
}

/// Ported from rift_extensions.feature: "JavaScript inject script with syntax error is rejected
/// at creation time"
#[tokio::test]
async fn javascript_inject_syntax_error_is_rejected_with_400() {
    let rift = Rift::start().await;
    let cfg = json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"responses": [{"inject": "function(config, state) { return { statusCode: "}]}]
    });
    assert_create_rejected(&rift, &cfg, &["Script validation failed"]).await;
}

/// Ported from rift_extensions.feature: "Valid Rhai script is accepted"
#[tokio::test]
async fn valid_rhai_script_is_accepted() {
    let rift = Rift::start().await;
    let cfg = script_imposter(
        rift.imposter,
        "rhai",
        "fn should_inject(request, flow_store) { #{ inject: false } }",
    );
    let resp = rift.post_imposter(&cfg).await;
    assert_eq!(resp.status().as_u16(), 201);
}

/// Ported from rift_extensions.feature: "Invalid script in stub addition is rejected"
#[tokio::test]
async fn invalid_script_in_stub_addition_is_rejected_with_400() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "OK"}}]}]
    }))
    .await;
    let resp = rift
        .client
        .post(rift.admin_url(&format!("/imposters/{}/stubs", rift.imposter)))
        .json(&json!({"stub": {"responses": [{"_rift": {"script": {
            "engine": "rhai", "code": "fn invalid_syntax { }"
        }}}]}}))
        .send()
        .await
        .expect("POST stub");
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.expect("error body");
    assert!(body.contains("Script validation failed"), "{body}");
}

/// Ported from rift_extensions.feature: "Unknown script engine is rejected"
#[tokio::test]
async fn unknown_script_engine_is_rejected_with_400() {
    let rift = Rift::start().await;
    let cfg = script_imposter(rift.imposter, "unknown_engine", "some code");
    assert_create_rejected(
        &rift,
        &cfg,
        &["Script validation failed", "Unknown script engine"],
    )
    .await;
}

// =============================================================================
// rift_extensions.feature: faults, empty _rift, header casing
// =============================================================================

/// Ported from rift_extensions.feature: "Error fault with custom headers"
#[tokio::test]
async fn error_fault_with_custom_headers() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"predicates": [], "responses": [{
            "is": {"statusCode": 200, "body": "normal"},
            "_rift": {"fault": {"error": {
                "probability": 1.0, "status": 429, "body": "Too Many Requests",
                "headers": {"Retry-After": "60", "X-RateLimit-Reset": "1234567890"}
            }}}
        }]}]
    }))
    .await;
    let resp = rift
        .client
        .get(rift.imposter_url(rift.imposter, "/"))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status().as_u16(), 429);
    assert_eq!(resp.headers()["retry-after"], "60");
    assert_eq!(resp.headers()["x-ratelimit-reset"], "1234567890");
    assert_eq!(resp.text().await.expect("body"), "Too Many Requests");
}

/// Ported from rift_extensions.feature: "Empty _rift config is ignored"
#[tokio::test]
async fn empty_rift_config_is_ignored() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http", "_rift": {},
        "stubs": [{"predicates": [], "responses": [{
            "is": {"statusCode": 200, "body": "works"}, "_rift": {}
        }]}]
    }))
    .await;
    let (status, body) = rift.get(&rift.imposter_url(rift.imposter, "/"), None).await;
    assert_eq!(status, 200);
    assert_eq!(body, "works");
}

/// Ported from rift_extensions.feature: "Per-client rate limiting using headers". Essential
/// engine behaviour: the request sends `X-Client-ID`, the script reads `X-Client-Id`. Today the
/// engine hands inject scripts Title-Case header names, so only the `X-Client-Id` spelling finds
/// the value; the sent spelling and the lowercase spelling are undefined.
#[tokio::test]
async fn inject_header_lookup_case() {
    let rift = Rift::start().await;
    rift.create(&json!({
        "port": rift.imposter, "protocol": "http",
        "stubs": [{"predicates": [], "responses": [{"inject":
            "function(config) { var h = config.request.headers; return { statusCode: 200, body: 'exact=' + h['X-Client-Id'] + ' sent=' + h['X-Client-ID'] + ' lower=' + h['x-client-id'] }; }"
        }]}]
    }))
    .await;
    let (status, body) = rift
        .get(
            &rift.imposter_url(rift.imposter, "/"),
            Some(("X-Client-ID", "client-a")),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, "exact=client-a sent=undefined lower=undefined");
}

// =============================================================================
// stub_management.feature: _rift.warnings
// =============================================================================

fn warning_types(created: &Value) -> Vec<String> {
    created["_rift"]["warnings"]
        .as_array()
        .expect("_rift.warnings is an array")
        .iter()
        .filter_map(|w| w["warningType"].as_str().map(str::to_string))
        .collect()
}

/// Ported from tests/compatibility/features/stub_management.feature: "Rift returns warnings for
/// shadowed stubs". The warnings are on the creation response itself.
#[tokio::test]
async fn warnings_for_shadowed_stubs() {
    let rift = Rift::start().await;
    let created = rift
        .create(&json!({
            "port": rift.imposter, "protocol": "http",
            "stubs": [
                {"predicates": [], "responses": [{"is": {"statusCode": 200, "body": "catch all"}}]},
                {"predicates": [{"equals": {"path": "/specific"}}], "responses": [{"is": {"statusCode": 200, "body": "specific"}}]}
            ]
        }))
        .await;
    assert!(warning_types(&created).contains(&"catch_all".to_string()));
}

/// Ported from stub_management.feature: "Rift returns warnings for duplicate IDs"
#[tokio::test]
async fn warnings_for_duplicate_ids() {
    let rift = Rift::start().await;
    let created = rift
        .create(&json!({
            "port": rift.imposter, "protocol": "http",
            "stubs": [
                {"id": "duplicate-id", "predicates": [{"equals": {"path": "/a"}}], "responses": [{"is": {"statusCode": 200, "body": "A"}}]},
                {"id": "duplicate-id", "predicates": [{"equals": {"path": "/b"}}], "responses": [{"is": {"statusCode": 200, "body": "B"}}]}
            ]
        }))
        .await;
    assert!(warning_types(&created).contains(&"duplicate_id".to_string()));
}
