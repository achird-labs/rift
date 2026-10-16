//! Issue #1518: Mountebank's `PUT /imposters` deletes every imposter and recreates each one in the
//! body from its JSON, so nothing survives it: response cycles, recorded requests and
//! `numberOfRequests`, injection state, saved `proxyOnce` responses. Rift reconciles instead (#549),
//! so a failure never loses what was running, and then resets the imposters in the body in place.
//! `?reconcile=true` keeps the old behaviour: an unchanged imposter keeps its state.

use std::net::TcpListener;
use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    url: String,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
            .with_allow_injection(true);
        let running = server.bind().await.expect("admin API binds");
        Admin {
            client: reqwest::Client::new(),
            url: format!("http://{}", running.local_addr()),
        }
    }

    /// Create `imposter` without a port and return it with the port the engine assigned, ready to
    /// be sent back in a `PUT /imposters` body.
    async fn create(&self, mut imposter: Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/imposters", self.url))
            .json(&imposter)
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
        let body: Value = response.json().await.expect("json");
        let port = body["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port");
        imposter["port"] = json!(port);
        (port, imposter)
    }

    async fn put(&self, imposters: &[&Value], reconcile: bool) -> (u16, Value) {
        let query = if reconcile { "?reconcile=true" } else { "" };
        self.put_query(imposters, query).await
    }

    async fn put_query(&self, imposters: &[&Value], query: &str) -> (u16, Value) {
        let response = self
            .client
            .put(format!("{}/imposters{query}", self.url))
            .json(&json!({ "imposters": imposters }))
            .send()
            .await
            .expect("PUT");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn imposter(&self, port: u16) -> Value {
        self.client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json")
    }

    /// `numberOfRequests` and the number of recorded requests.
    async fn journal(&self, port: u16) -> (u64, usize) {
        let imposter = self.imposter(port).await;
        (
            imposter["numberOfRequests"]
                .as_u64()
                .expect("numberOfRequests"),
            imposter["requests"].as_array().map_or(0, Vec::len),
        )
    }
}

/// A fresh client per call: no pooled connection outlives a step.
async fn get(port: u16, path: &str) -> String {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body")
}

async fn bodies(port: u16, path: &str, n: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(get(port, path).await);
    }
    out
}

fn is(body: &str) -> Value {
    json!({ "is": { "body": body } })
}

fn cycle(path: &str, responses: &[&str]) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": responses.iter().map(|b| is(b)).collect::<Vec<_>>(),
    })
}

/// The probe pair from the triage: 4711 (`/x → A,B,C`, `/y → P,Q,R`) and 4712 (`/x → K,L`).
async fn probe_pair(admin: &Admin) -> ((u16, Value), (u16, Value)) {
    let first = admin
        .create(
            json!({ "protocol": "http", "recordRequests": true, "stubs": [
                cycle("/x", &["A", "B", "C"]),
                cycle("/y", &["P", "Q", "R"]),
            ]}),
        )
        .await;
    let second = admin
        .create(
            json!({ "protocol": "http", "recordRequests": true, "stubs": [
                cycle("/x", &["K", "L"]),
            ]}),
        )
        .await;
    (first, second)
}

fn report_ports(body: &Value, list: &str) -> Vec<u64> {
    body[list]
        .as_array()
        .map(|ports| ports.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// The same document again: every cycle and journal starts over. Under `?reconcile=true` nothing
/// changed, so nothing is touched.
async fn same_document(reconcile: bool) {
    let admin = Admin::start().await;
    let ((a, a_doc), (b, b_doc)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);
    assert_eq!(bodies(b, "/x", 1).await, ["K"]);

    let (status, body) = admin.put(&[&a_doc, &b_doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    let listed = body["imposters"]
        .as_array()
        .expect("imposters")
        .iter()
        .find(|i| i["port"] == json!(a))
        .expect("first imposter listed");
    if reconcile {
        assert_eq!(listed["numberOfRequests"], 2, "{body}");
        assert_eq!(admin.journal(a).await, (2, 2));
        assert_eq!(bodies(a, "/x", 3).await, ["C", "A", "B"]);
        assert_eq!(bodies(b, "/x", 1).await, ["L"]);
    } else {
        assert_eq!(
            listed["numberOfRequests"], 0,
            "the 200 body is rendered after the reset"
        );
        assert_eq!(admin.journal(a).await, (0, 0));
        assert_eq!(admin.journal(b).await, (0, 0));
        assert_eq!(bodies(a, "/x", 3).await, ["A", "B", "C"]);
        assert_eq!(bodies(b, "/x", 1).await, ["K"]);
    }
}

#[tokio::test]
async fn put_of_the_same_document_restarts_cycles_and_journal() {
    same_document(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_keeps_cycles_and_journal() {
    same_document(true).await;
}

/// A changed stub is a stub patch in Rift's reconcile; the untouched stubs and the journal used to
/// keep their state through it.
async fn one_stub_changed(reconcile: bool) {
    let admin = Admin::start().await;
    let ((a, mut a_doc), (_, b_doc)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);

    a_doc["stubs"][1] = cycle("/y", &["P2", "Q2"]);
    let (status, body) = admin.put(&[&a_doc, &b_doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(report_ports(&body, "stubPatched"), [u64::from(a)], "{body}");
    if reconcile {
        assert_eq!(admin.journal(a).await, (2, 2));
        assert_eq!(bodies(a, "/x", 1).await, ["C"]);
    } else {
        assert_eq!(admin.journal(a).await, (0, 0));
        assert_eq!(bodies(a, "/x", 1).await, ["A"]);
    }
    assert_eq!(bodies(a, "/y", 1).await, ["P2"]);
}

#[tokio::test]
async fn put_resets_a_stub_patched_imposter() {
    one_stub_changed(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_keeps_a_stub_patched_imposters_state() {
    one_stub_changed(true).await;
}

/// An `enabled`-only change toggles in place (#817). The second `PUT` re-enables under
/// `?reconcile=true` so the cursor can be read without resetting it again.
async fn enabled_toggled(reconcile: bool) {
    let admin = Admin::start().await;
    let ((a, mut a_doc), (_, b_doc)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);

    a_doc["enabled"] = json!(false);
    let (status, body) = admin.put(&[&a_doc, &b_doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(report_ports(&body, "toggled"), [u64::from(a)], "{body}");
    let expected_journal = if reconcile { (2, 2) } else { (0, 0) };
    assert_eq!(admin.journal(a).await, expected_journal);

    a_doc["enabled"] = json!(true);
    let (status, body) = admin.put(&[&a_doc, &b_doc], true).await;
    assert_eq!(status, 200, "{body}");
    let expected = if reconcile { "C" } else { "A" };
    assert_eq!(bodies(a, "/x", 1).await, [expected]);
}

#[tokio::test]
async fn put_resets_an_enabled_toggled_imposter() {
    enabled_toggled(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_keeps_an_enabled_toggled_imposters_state() {
    enabled_toggled(true).await;
}

/// A JavaScript `inject` response's `config.state` starts over, as on a Mountebank recreate.
#[cfg(feature = "javascript")]
async fn injection_state(reconcile: bool) {
    let admin = Admin::start().await;
    let (port, doc) = admin
        .create(json!({ "protocol": "http", "stubs": [{ "responses": [{ "inject":
            "function (config) { config.state.n = (config.state.n || 0) + 1; return { body: String(config.state.n) }; }"
        }]}]}))
        .await;
    assert_eq!(bodies(port, "/", 2).await, ["1", "2"]);

    let (status, body) = admin.put(&[&doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    let expected = if reconcile { "3" } else { "1" };
    assert_eq!(bodies(port, "/", 1).await, [expected]);
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn put_restarts_injection_state() {
    injection_state(false).await;
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn put_with_reconcile_true_keeps_injection_state() {
    injection_state(true).await;
}

/// Rift's scenario state lives in the flow store, which a fresh create starts empty.
async fn scenario_state(reconcile: bool) {
    let admin = Admin::start().await;
    let (port, doc) = admin
        .create(json!({ "protocol": "http", "stubs": [
            { "scenarioName": "s", "requiredScenarioState": "Started", "newScenarioState": "Second",
              "responses": [is("first")] },
            { "scenarioName": "s", "requiredScenarioState": "Second",
              "responses": [is("second")] },
        ]}))
        .await;
    assert_eq!(bodies(port, "/", 2).await, ["first", "second"]);

    let (status, body) = admin.put(&[&doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    let expected = if reconcile { "second" } else { "first" };
    assert_eq!(bodies(port, "/", 1).await, [expected]);
}

#[tokio::test]
async fn put_returns_a_scenario_to_started() {
    scenario_state(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_keeps_scenario_state() {
    scenario_state(true).await;
}

/// A `proxyOnce` without generators saves its response in Rift's proxy store, not as a stub, so
/// the body cannot carry it; Mountebank's recreate drops it and proxies again.
async fn saved_proxy_response(reconcile: bool) {
    // The upstream lives under its own admin server, so the PUT under test cannot touch it.
    let upstream_admin = Admin::start().await;
    let (upstream, _) = upstream_admin
        .create(
            json!({ "protocol": "http", "recordRequests": true, "stubs": [{ "responses": [
                is("U1"), is("U2"), is("U3"),
            ]}]}),
        )
        .await;
    let admin = Admin::start().await;
    let (port, doc) = admin
        .create(
            json!({ "protocol": "http", "stubs": [{ "responses": [{ "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyOnce"
            }}]}]}),
        )
        .await;
    assert_eq!(bodies(port, "/s", 2).await, ["U1", "U1"]);
    assert_eq!(upstream_admin.journal(upstream).await.1, 1);

    let (status, body) = admin.put(&[&doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    if reconcile {
        assert_eq!(bodies(port, "/s", 1).await, ["U1"]);
        assert_eq!(upstream_admin.journal(upstream).await.1, 1);
    } else {
        assert_eq!(bodies(port, "/s", 1).await, ["U2"]);
        assert_eq!(upstream_admin.journal(upstream).await.1, 2);
    }
}

#[tokio::test]
async fn put_drops_saved_proxy_once_responses() {
    saved_proxy_response(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_keeps_saved_proxy_once_responses() {
    saved_proxy_response(true).await;
}

/// A port another process holds fails; the ports that applied are reset and keep serving. Rift
/// does not copy Mountebank's pre-delete, which leaves nothing running on this failure.
async fn partial_failure(reconcile: bool) {
    let admin = Admin::start().await;
    let ((a, a_doc), (b, b_doc)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);
    let held = TcpListener::bind("127.0.0.1:0").expect("bind");
    let held_port = held.local_addr().expect("addr").port();
    let squatted = json!({ "protocol": "http", "port": held_port, "host": "127.0.0.1" });

    let (status, body) = admin.put(&[&a_doc, &b_doc, &squatted], reconcile).await;
    assert_eq!(status, 400, "{body}");
    let failed = body["failed"].as_array().expect("failed");
    assert_eq!(failed.len(), 1, "{body}");
    assert!(
        failed[0]
            .as_str()
            .expect("failed entry")
            .starts_with(&format!("{held_port}: ")),
        "{body}"
    );
    if reconcile {
        assert_eq!(admin.journal(a).await, (2, 2));
        assert_eq!(bodies(a, "/x", 1).await, ["C"]);
    } else {
        assert_eq!(admin.journal(a).await, (0, 0));
        assert_eq!(bodies(a, "/x", 1).await, ["A"]);
    }
    assert_eq!(bodies(b, "/x", 1).await, ["K"]);
}

#[tokio::test]
async fn put_partial_failure_resets_the_ports_that_applied() {
    partial_failure(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_partial_failure_keeps_state() {
    partial_failure(true).await;
}

const SESSION: &str = "X-Session";

fn conditional(predicate_path: Option<&str>, body: &str) -> Value {
    let mut stub = json!({
        "responses": [{ "is": { "body": body }, "_rift": { "conditional": true } }]
    });
    if let Some(path) = predicate_path {
        stub["predicates"] = json!([{ "equals": { "path": path } }]);
    }
    stub
}

fn gated(state: &str, body: &str) -> Value {
    let mut stub = conditional(None, body);
    stub["scenarioName"] = json!("flag");
    stub["requiredScenarioState"] = json!(state);
    stub
}

async fn conditional_get(port: u16, path: &str) -> (String, i64) {
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header(SESSION, "s1")
        .send()
        .await
        .expect("request");
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            chrono::DateTime::parse_from_rfc2822(v)
                .expect("an HTTP-date")
                .timestamp()
        })
        .expect("Last-Modified");
    (response.text().await.expect("body"), last_modified)
}

/// `_rift.conditional` (#1280, #1307): the reset leaves an unchanged stub's `Last-Modified` alone
/// (#1294) and moves the stamp of a stub whose answer the scenario reset changed.
async fn conditional_stamps(reconcile: bool) {
    let admin = Admin::start().await;
    let (port, doc) = admin
        .create(json!({ "protocol": "http",
        "_rift": { "flowState": { "flowIdSource": format!("header:{SESSION}") } },
        "stubs": [
            conditional(Some("/u"), "unchanged"),
            { "predicates": [{ "equals": { "path": "/flip" } }],
              "scenarioName": "flag", "requiredScenarioState": "Started",
              "newScenarioState": "Flipped", "responses": [{ "is": { "statusCode": 204 } }] },
            gated("Started", "v1"),
            gated("Flipped", "v2"),
        ]}))
        .await;
    let (_, unchanged_before) = conditional_get(port, "/u").await;
    get_with_session(port, "/flip").await;
    let (answer, flipped_at) = conditional_get(port, "/g").await;
    assert_eq!(answer, "v2");

    let (status, body) = admin.put(&[&doc], reconcile).await;
    assert_eq!(status, 200, "{body}");
    let (_, unchanged_after) = conditional_get(port, "/u").await;
    assert_eq!(
        unchanged_after, unchanged_before,
        "unchanged content keeps its stamp"
    );
    let (answer, stamp) = conditional_get(port, "/g").await;
    if reconcile {
        assert_eq!((answer.as_str(), stamp), ("v2", flipped_at));
    } else {
        assert_eq!(answer, "v1");
        assert!(stamp > flipped_at, "{stamp} after {flipped_at}");
    }
}

async fn get_with_session(port: u16, path: &str) {
    let status = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header(SESSION, "s1")
        .send()
        .await
        .expect("request")
        .status();
    assert_eq!(status.as_u16(), 204);
}

#[tokio::test]
async fn put_restamps_a_stub_behind_a_reset_scenario_only() {
    conditional_stamps(false).await;
}

#[tokio::test]
async fn put_with_reconcile_true_moves_no_stamp() {
    conditional_stamps(true).await;
}

#[tokio::test]
async fn put_with_reconcile_false_is_mountebank_semantics() {
    let admin = Admin::start().await;
    let ((a, a_doc), (_, b_doc)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);
    let (status, body) = admin.put_query(&[&a_doc, &b_doc], "?reconcile=false").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(bodies(a, "/x", 1).await, ["A"]);
}

#[tokio::test]
async fn put_with_an_unknown_reconcile_value_is_a_400_and_changes_nothing() {
    let admin = Admin::start().await;
    let ((a, a_doc), (b, _)) = probe_pair(&admin).await;
    assert_eq!(bodies(a, "/x", 2).await, ["A", "B"]);
    let (status, body) = admin.put_query(&[&a_doc], "?reconcile=yes").await;
    assert_eq!(status, 400, "{body}");
    let message = body["errors"][0]["message"].as_str().expect("message");
    assert!(message.contains("reconcile"), "{body}");
    assert_eq!(admin.journal(a).await, (2, 2));
    assert_eq!(bodies(a, "/x", 1).await, ["C"]);
    assert_eq!(
        admin.imposter(b).await["port"],
        json!(b),
        "the imposter left out of the body is still running"
    );
}
