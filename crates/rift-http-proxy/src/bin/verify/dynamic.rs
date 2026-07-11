//! Dynamic-behavior assertion for `rift-verify` (issue #251).
//!
//! The verifier normally SKIPs dynamic stubs (proxy / inject / fault / cycling / request-derived
//! behaviors) because their output is not a static function of the stub. This module makes them
//! assertable (opt-in via `--verify-dynamic`) through three complementary mechanisms:
//!
//! 1. an embedded mock upstream → verifies `proxy` (record/replay + recorded-stub prepend);
//! 2. a `_verify` expectation sequence run against a FRESH imposter (clean cyclic/FSM state) →
//!    verifies inject/script/decorate/copy/lookup/cycling/repeat/stateful FSM;
//! 3. transport-aware `_rift.fault` assertions (probability 1.0) → tcp reset / latency / error.
//!
//! Each mechanism recreates a throwaway imposter on a free port via the admin API and tears it
//! down afterward, so it never mutates the imposters under test.

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

use super::is_transport_reset_error;

/// One check produced by the dynamic verifier: a pass, a fail, or a visible skip (a dynamic stub
/// the verifier recognised but cannot assert — surfaced so it is never silently ignored).
pub struct DynCheck {
    pub label: String,
    pub passed: bool,
    pub skipped: bool,
    pub detail: String,
}

impl DynCheck {
    fn pass(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: true,
            skipped: false,
            detail: String::new(),
        }
    }
    fn fail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: false,
            skipped: false,
            detail: detail.into(),
        }
    }
    fn skip(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: false,
            skipped: true,
            detail: detail.into(),
        }
    }
}

/// True when a stub is dynamic (its output is not a static function of the stub) but carries none
/// of the assertable markers (`_verify`, a real `proxy`, or a deterministic `_rift.fault`). Such a
/// stub must be surfaced as a visible skip rather than silently ignored under `--verify-dynamic`.
fn is_dynamic_unassertable(stub: &serde_json::Value) -> bool {
    if parse_verify_spec(stub).is_some() || first_proxy(stub).is_some() {
        return false;
    }
    if first_response(stub)
        .as_ref()
        .and_then(fault_expectation)
        .is_some()
    {
        return false;
    }
    let responses = stub
        .get("responses")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    super::check_if_dynamic(&responses).0
}

// ============================================================================
// `_verify` expectation schema (mechanism 2) — pure
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct VerifySpec {
    #[serde(default)]
    pub sequence: Vec<VerifyStep>,
}

#[derive(Debug, Deserialize)]
pub struct VerifyStep {
    pub request: VerifyRequest,
    pub expect: VerifyExpect,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    #[serde(default = "default_method")]
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_method() -> String {
    "GET".to_string()
}

#[derive(Debug, Deserialize)]
pub struct VerifyExpect {
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default, rename = "bodyContains")]
    pub body_contains: Option<String>,
    #[serde(default, rename = "bodyEquals")]
    pub body_equals: Option<String>,
}

/// Parse the engine-preserved `_verify` annotation off a stub (issue #251). Returns `None` when
/// the stub carries no annotation; `Some(Err)` when it is present but malformed.
pub fn parse_verify_spec(stub: &serde_json::Value) -> Option<Result<VerifySpec, String>> {
    let raw = stub.get("_verify")?;
    Some(serde_json::from_value(raw.clone()).map_err(|e| format!("invalid _verify: {e}")))
}

/// A single mismatch between an observed response and a declared [`VerifyExpect`].
/// `Display` reproduces the human-readable reason shown in the verification report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpectMismatch {
    #[error("status {actual} != expected {expected}")]
    Status { actual: u16, expected: u16 },
    #[error("body {actual:?} != expected {expected:?}")]
    BodyEquals { actual: String, expected: String },
    #[error("body {actual:?} does not contain {expected:?}")]
    BodyContains { actual: String, expected: String },
}

/// Check an observed (status, body) against a declared expectation. Returns `Ok(())` on a match
/// or `Err` describing the first mismatch.
pub fn check_expect(expect: &VerifyExpect, status: u16, body: &str) -> Result<(), ExpectMismatch> {
    if let Some(want) = expect.status
        && status != want
    {
        return Err(ExpectMismatch::Status {
            actual: status,
            expected: want,
        });
    }
    if let Some(want) = &expect.body_equals
        && body != want
    {
        return Err(ExpectMismatch::BodyEquals {
            actual: body.to_string(),
            expected: want.clone(),
        });
    }
    if let Some(want) = &expect.body_contains
        && !body.contains(want.as_str())
    {
        return Err(ExpectMismatch::BodyContains {
            actual: body.to_string(),
            expected: want.clone(),
        });
    }
    Ok(())
}

// ============================================================================
// Fault expectations (mechanism 3) — pure
// ============================================================================

/// The asserted outcome of a deterministic `_rift.fault` (probability 1.0).
#[derive(Debug, PartialEq, Eq)]
pub enum FaultExpectation {
    /// `tcp` → the connection drops at the transport layer.
    TransportReset,
    /// `latency` → the response is delayed by at least this many milliseconds.
    Latency { ms: u64 },
    /// `error` → the response carries this status code.
    ErrorStatus { status: u16 },
}

/// Derive the deterministic fault expectation from a stub response's `_rift.fault` block, or
/// `None` when there is no fault or it is probabilistic (`probability < 1.0`, non-asserted by
/// design). `tcp` is deterministic in its bare string form and its object form with
/// `probability >= 1.0`; a probabilistic `tcp` object (issue #531) is non-asserted like the others.
pub fn fault_expectation(response: &serde_json::Value) -> Option<FaultExpectation> {
    let fault = response.get("_rift")?.get("fault")?;
    if let Some(tcp) = fault.get("tcp") {
        return is_certain(tcp).then_some(FaultExpectation::TransportReset);
    }
    if let Some(latency) = fault.get("latency")
        && is_certain(latency)
    {
        let ms = latency.get("ms").and_then(|v| v.as_u64())?;
        return Some(FaultExpectation::Latency { ms });
    }
    if let Some(error) = fault.get("error")
        && is_certain(error)
    {
        let status = error.get("status").and_then(|v| v.as_u64())? as u16;
        return Some(FaultExpectation::ErrorStatus { status });
    }
    None
}

/// A fault sub-config is asserted only when it is certain to fire: `probability` absent (treated
/// as always) or `>= 1.0`. Also used by `expects_tcp_fault` in the parent module for the `tcp`
/// fault (a bare string has no `probability`, so it reads as certain).
pub fn is_certain(fault_part: &serde_json::Value) -> bool {
    match fault_part.get("probability") {
        None => true,
        Some(p) => p.as_f64().map(|p| p >= 1.0).unwrap_or(false),
    }
}

/// A measured latency satisfies a configured delay when it is at least the configured value.
/// (Network/scheduling overhead only ever makes the observed delay larger.)
pub fn latency_meets_threshold(elapsed_ms: u128, configured_ms: u64) -> bool {
    elapsed_ms >= u128::from(configured_ms)
}

// ============================================================================
// Proxy record/replay (mechanism 1) — pure
// ============================================================================

/// True when a proxy response records and prepends a stub on first contact — i.e. `proxyOnce`/
/// `proxyAlways` with a non-empty `predicateGenerators` (issue #251 / conformance.sh). Without
/// `predicateGenerators` the engine replays internally but does not prepend a stub.
pub fn proxy_records_stub(proxy: &serde_json::Value) -> bool {
    let mode = proxy
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("proxyOnce");
    let records = matches!(mode, "proxyOnce" | "proxyAlways");
    let has_generators = proxy
        .get("predicateGenerators")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    records && has_generators
}

// ============================================================================
// Live runner
// ============================================================================

/// Shared context for the live dynamic checks.
pub struct DynamicVerifier<'a> {
    pub client: &'a Client,
    pub admin_url: &'a str,
}

impl<'a> DynamicVerifier<'a> {
    /// Verify every dynamic stub in one imposter, returning the asserted checks. `imposter` is the
    /// raw JSON from `GET /imposters/:port` (so all engine-preserved fields are available).
    pub async fn verify_imposter(&self, imposter: &serde_json::Value) -> Vec<DynCheck> {
        let mut checks = Vec::new();
        let stubs = imposter.get("stubs").and_then(|v| v.as_array());

        for (idx, stub) in stubs.into_iter().flatten().enumerate() {
            if let Some(spec) = parse_verify_spec(stub) {
                checks.extend(self.run_verify_sequence(imposter, idx, spec).await);
            } else if let Some(proxy) = first_proxy(stub) {
                checks.extend(self.run_proxy_check(idx, &proxy).await);
            } else if let Some((resp, exp)) =
                first_response(stub).and_then(|r| fault_expectation(&r).map(|e| (r, e)))
            {
                checks.extend(self.run_fault_check(idx, &resp, exp).await);
            } else if is_dynamic_unassertable(stub) {
                checks.push(DynCheck::skip(
                    format!("stub #{idx} dynamic"),
                    "no `_verify` annotation; cannot assert (add `_verify` to verify it)"
                        .to_string(),
                ));
            }
        }
        checks
    }

    /// Mechanism 2: recreate the imposter on a fresh port (clean cyclic/FSM state) and drive the
    /// declared `_verify` sequence against it.
    async fn run_verify_sequence(
        &self,
        imposter: &serde_json::Value,
        stub_idx: usize,
        spec: Result<VerifySpec, String>,
    ) -> Vec<DynCheck> {
        let label = format!("stub #{stub_idx} _verify");
        let spec = match spec {
            Ok(s) => s,
            Err(e) => return vec![DynCheck::fail(label, e)],
        };
        let Some(port) = free_port() else {
            return vec![DynCheck::fail(label, "no free port".to_string())];
        };
        let mut config = imposter.clone();
        config["port"] = port.into();
        if let Err(e) = self.create_imposter(&config).await {
            return vec![DynCheck::fail(label, e)];
        }

        let mut checks = Vec::new();
        for (i, step) in spec.sequence.iter().enumerate() {
            let step_label = format!("{label}[{i}] {} {}", step.request.method, step.request.path);
            match self.drive_step(port, &step.request).await {
                Ok((status, body)) => match check_expect(&step.expect, status, &body) {
                    Ok(()) => checks.push(DynCheck::pass(step_label)),
                    Err(why) => checks.push(DynCheck::fail(step_label, why.to_string())),
                },
                Err(e) => checks.push(DynCheck::fail(step_label, e)),
            }
        }

        self.delete_imposter(port).await;
        checks
    }

    /// Mechanism 1: stand up a deterministic mock upstream, recreate the proxy stub pointing at it,
    /// assert the proxied sentinel and (when `predicateGenerators` is set) the recorded-stub prepend.
    async fn run_proxy_check(&self, stub_idx: usize, proxy: &serde_json::Value) -> Vec<DynCheck> {
        let label = format!("stub #{stub_idx} proxy");
        let mode = proxy
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("proxyOnce")
            .to_string();

        let mock = match MockUpstream::spawn().await {
            Ok(m) => m,
            Err(e) => return vec![DynCheck::fail(label, e)],
        };
        let Some(port) = free_port() else {
            return vec![DynCheck::fail(label, "no free port".to_string())];
        };

        let mut proxy_cfg = serde_json::json!({ "to": mock.url(), "mode": mode });
        if let Some(pg) = proxy.get("predicateGenerators") {
            proxy_cfg["predicateGenerators"] = pg.clone();
        }
        let config = serde_json::json!({
            "port": port, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": proxy_cfg }] }],
        });
        if let Err(e) = self.create_imposter(&config).await {
            return vec![DynCheck::fail(label, e)];
        }

        let mut checks = Vec::new();
        let before = self.stub_count(port).await;
        match self
            .drive_step(port, &get_request("/__rift_verify_probe"))
            .await
        {
            Ok((status, body)) if status == 200 && body == MockUpstream::SENTINEL => {
                checks.push(DynCheck::pass(format!("{label} proxied sentinel")));
            }
            Ok((status, body)) => checks.push(DynCheck::fail(
                format!("{label} proxied sentinel"),
                format!("got status {status} body {body:?}"),
            )),
            Err(e) => checks.push(DynCheck::fail(format!("{label} proxied sentinel"), e)),
        }

        if proxy_records_stub(proxy) {
            let after = self.stub_count(port).await;
            match (before, after) {
                (Some(b), Some(a)) if a == b + 1 => {
                    checks.push(DynCheck::pass(format!("{label} records stub (+1)")))
                }
                (Some(b), Some(a)) => checks.push(DynCheck::fail(
                    format!("{label} records stub (+1)"),
                    format!("stub count {b} -> {a}, expected +1"),
                )),
                _ => checks.push(DynCheck::fail(
                    format!("{label} records stub (+1)"),
                    "could not read stub count".to_string(),
                )),
            }
        }

        self.delete_imposter(port).await;
        mock.shutdown();
        checks
    }

    /// Mechanism 3: recreate the single fault stub and assert the transport-aware outcome.
    async fn run_fault_check(
        &self,
        stub_idx: usize,
        response: &serde_json::Value,
        expectation: FaultExpectation,
    ) -> Vec<DynCheck> {
        let label = format!("stub #{stub_idx} fault");
        let Some(port) = free_port() else {
            return vec![DynCheck::fail(label, "no free port".to_string())];
        };
        let path = "/__rift_verify_fault";
        let config = serde_json::json!({
            "port": port, "protocol": "http",
            "stubs": [{ "predicates": [{ "equals": { "path": path } }], "responses": [response] }],
        });
        if let Err(e) = self.create_imposter(&config).await {
            return vec![DynCheck::fail(label, e)];
        }

        // A non-fault control path proves the imposter is actually serving, so a reset/delay on the
        // fault path is attributable to the fault rather than a sick imposter or ordinary overhead.
        let control_path = "/__rift_verify_health";
        let check = match expectation {
            FaultExpectation::TransportReset => {
                match self.drive_step(port, &get_request(control_path)).await {
                    Err(e) => DynCheck::fail(
                        format!("{label} tcp reset"),
                        format!("control request failed (imposter not serving): {e}"),
                    ),
                    Ok(_) => match self.drive_step(port, &get_request(path)).await {
                        Err(e) if is_transport_reset_error(&e) => {
                            DynCheck::pass(format!("{label} tcp reset"))
                        }
                        Err(e) => DynCheck::fail(
                            format!("{label} tcp reset"),
                            format!("non-reset error: {e}"),
                        ),
                        Ok((status, _)) => DynCheck::fail(
                            format!("{label} tcp reset"),
                            format!("expected reset, got HTTP {status}"),
                        ),
                    },
                }
            }
            FaultExpectation::Latency { ms } => {
                let base_start = std::time::Instant::now();
                let baseline = self.drive_step(port, &get_request(control_path)).await;
                let baseline_ms = base_start.elapsed().as_millis();
                match baseline {
                    Err(e) => DynCheck::fail(
                        format!("{label} latency"),
                        format!("control request failed: {e}"),
                    ),
                    Ok(_) => {
                        let start = std::time::Instant::now();
                        let outcome = self.drive_step(port, &get_request(path)).await;
                        let elapsed = start.elapsed().as_millis();
                        let delay_shown = elapsed.saturating_sub(baseline_ms) * 2 >= u128::from(ms);
                        match outcome {
                            Err(e) => DynCheck::fail(format!("{label} latency"), e),
                            Ok(_) if latency_meets_threshold(elapsed, ms) && delay_shown => {
                                DynCheck::pass(format!(
                                    "{label} latency >= {ms}ms ({elapsed}ms over {baseline_ms}ms baseline)"
                                ))
                            }
                            Ok(_) => DynCheck::fail(
                                format!("{label} latency"),
                                format!(
                                    "{elapsed}ms over {baseline_ms}ms baseline does not show the configured {ms}ms delay"
                                ),
                            ),
                        }
                    }
                }
            }
            FaultExpectation::ErrorStatus { status: want } => {
                // If the base `is` status already equals the fault status, a non-firing fault would
                // still return `want` — the check could not tell them apart, so skip it visibly.
                let base_status = response
                    .get("is")
                    .and_then(|is| is.get("statusCode"))
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .map(|n| n as u16);
                if base_status == Some(want) {
                    DynCheck::skip(
                        format!("stub #{stub_idx} fault"),
                        format!(
                            "error-fault status {want} equals the base response status; cannot prove the fault fired"
                        ),
                    )
                } else {
                    match self.drive_step(port, &get_request(path)).await {
                        Ok((status, _)) if status == want => {
                            DynCheck::pass(format!("{label} error status {want}"))
                        }
                        Ok((status, _)) => DynCheck::fail(
                            format!("{label} error status"),
                            format!("got {status}, expected {want}"),
                        ),
                        Err(e) => DynCheck::fail(format!("{label} error status"), e),
                    }
                }
            }
        };

        self.delete_imposter(port).await;
        vec![check]
    }

    async fn drive_step(
        &self,
        port: u16,
        request: &VerifyRequest,
    ) -> Result<(u16, String), String> {
        let url = format!("http://127.0.0.1:{port}{}", request.path);
        let mut req = match request.method.to_uppercase().as_str() {
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            "PATCH" => self.client.patch(&url),
            "HEAD" => self.client.head(&url),
            _ => self.client.get(&url),
        };
        for (k, v) in &request.headers {
            req = req.header(k, v);
        }
        if let Some(body) = &request.body {
            req = req.body(body.clone());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| super::error_chain_string(&e))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| super::error_chain_string(&e))?;
        Ok((status, body))
    }

    async fn create_imposter(&self, config: &serde_json::Value) -> Result<(), String> {
        let resp = self
            .client
            .post(format!("{}/imposters", self.admin_url))
            .json(config)
            .send()
            .await
            .map_err(|e| format!("create imposter: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("create imposter: HTTP {}", resp.status().as_u16()))
        }
    }

    async fn delete_imposter(&self, port: u16) {
        if let Err(e) = self
            .client
            .delete(format!("{}/imposters/{port}", self.admin_url))
            .send()
            .await
        {
            eprintln!("warning: failed to delete throwaway imposter {port}: {e}");
        }
    }

    async fn stub_count(&self, port: u16) -> Option<usize> {
        let resp = self
            .client
            .get(format!("{}/imposters/{port}", self.admin_url))
            .send()
            .await
            .ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        Some(body.get("stubs")?.as_array()?.len())
    }
}

fn first_response(stub: &serde_json::Value) -> Option<serde_json::Value> {
    stub.get("responses")?.as_array()?.first().cloned()
}

fn first_proxy(stub: &serde_json::Value) -> Option<serde_json::Value> {
    let proxy = first_response(stub)?.get("proxy")?.clone();
    (proxy.is_object() && proxy.get("to").is_some()).then_some(proxy)
}

fn get_request(path: &str) -> VerifyRequest {
    VerifyRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        body: None,
        headers: HashMap::new(),
    }
}

/// Acquire a free TCP port by binding to :0 and immediately releasing it. There is an inherent
/// race between release and reuse, but the verifier owns the throwaway port for its short lifetime.
fn free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|a| a.port())
}

// ============================================================================
// Embedded mock upstream — a deterministic HTTP/1.1 server returning a sentinel
// ============================================================================

struct MockUpstream {
    port: u16,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl MockUpstream {
    const SENTINEL: &'static str = "rift-verify-upstream-sentinel";

    async fn spawn() -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("mock upstream bind: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("mock upstream addr: {e}"))?
            .port();
        let (tx, mut rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    accepted = listener.accept() => {
                        if let Ok((mut socket, _)) = accepted {
                            tokio::spawn(async move {
                                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                                let mut buf = [0u8; 1024];
                                let _ = socket.read(&mut buf).await;
                                let body = MockUpstream::SENTINEL;
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    body.len(),
                                    body
                                );
                                let _ = socket.write_all(response.as_bytes()).await;
                                let _ = socket.flush().await;
                            });
                        }
                    }
                }
            }
        });

        // Give the listener a moment to be ready for connections.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(Self { port, shutdown: tx })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn shutdown(self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── AC1: _verify schema parse ──────────────────────────────────────────
    #[test]
    fn verify_spec_parses() {
        let stub = json!({ "_verify": { "sequence": [
            { "request": { "method": "GET", "path": "/r" }, "expect": { "status": 503 } },
            { "request": { "path": "/r" }, "expect": { "status": 200, "bodyContains": "ok" } }
        ]}});
        let spec = parse_verify_spec(&stub).unwrap().unwrap();
        assert_eq!(spec.sequence.len(), 2);
        assert_eq!(spec.sequence[0].request.method, "GET");
        assert_eq!(spec.sequence[1].request.method, "GET"); // defaulted
        assert_eq!(spec.sequence[1].request.path, "/r");
        assert_eq!(spec.sequence[1].expect.body_contains.as_deref(), Some("ok"));
    }

    #[test]
    fn parse_verify_spec_none_when_absent() {
        assert!(parse_verify_spec(&json!({ "responses": [] })).is_none());
    }

    #[test]
    fn parse_verify_spec_err_when_malformed() {
        // `sequence` step missing the required `request` field.
        let stub = json!({ "_verify": { "sequence": [{ "expect": { "status": 200 } }] } });
        assert!(parse_verify_spec(&stub).unwrap().is_err());
    }

    // ── AC2: expect matcher ────────────────────────────────────────────────
    #[test]
    fn check_expect_status_and_body() {
        let expect: VerifyExpect =
            serde_json::from_value(json!({ "status": 200, "bodyContains": "id=77" })).unwrap();
        assert!(check_expect(&expect, 200, "order id=77 ok").is_ok());
        assert!(check_expect(&expect, 503, "order id=77 ok").is_err());
        assert!(check_expect(&expect, 200, "missing").is_err());
    }

    #[test]
    fn check_expect_body_equals_is_exact() {
        let expect: VerifyExpect =
            serde_json::from_value(json!({ "bodyEquals": "exact" })).unwrap();
        assert!(check_expect(&expect, 200, "exact").is_ok());
        assert!(check_expect(&expect, 200, "exact ").is_err());
    }

    #[test]
    fn check_expect_empty_matches_anything() {
        let expect: VerifyExpect = serde_json::from_value(json!({})).unwrap();
        assert!(check_expect(&expect, 418, "whatever").is_ok());
    }

    // ── AC3: fault expectation derivation ──────────────────────────────────
    #[test]
    fn fault_expectation_tcp() {
        let r = json!({ "is": { "statusCode": 200 }, "_rift": { "fault": { "tcp": "CONNECTION_RESET_BY_PEER" } } });
        assert_eq!(
            fault_expectation(&r),
            Some(FaultExpectation::TransportReset)
        );
    }

    // Issue #531: the object tcp form asserts a reset only when it is certain (p >= 1.0); a
    // probabilistic object (p < 1.0) is non-deterministic and yields no expectation.
    #[test]
    fn fault_expectation_tcp_object_form() {
        let certain =
            json!({ "_rift": { "fault": { "tcp": { "probability": 1.0, "type": "reset" } } } });
        assert_eq!(
            fault_expectation(&certain),
            Some(FaultExpectation::TransportReset)
        );
        let probabilistic =
            json!({ "_rift": { "fault": { "tcp": { "probability": 0.1, "type": "reset" } } } });
        assert_eq!(fault_expectation(&probabilistic), None);
    }

    #[test]
    fn fault_expectation_latency_and_error_when_certain() {
        let lat = json!({ "_rift": { "fault": { "latency": { "probability": 1.0, "ms": 700 } } } });
        assert_eq!(
            fault_expectation(&lat),
            Some(FaultExpectation::Latency { ms: 700 })
        );
        let err =
            json!({ "_rift": { "fault": { "error": { "probability": 1.0, "status": 503 } } } });
        assert_eq!(
            fault_expectation(&err),
            Some(FaultExpectation::ErrorStatus { status: 503 })
        );
    }

    #[test]
    fn fault_expectation_none_for_probabilistic_or_absent() {
        let prob =
            json!({ "_rift": { "fault": { "latency": { "probability": 0.5, "ms": 700 } } } });
        assert_eq!(fault_expectation(&prob), None);
        assert_eq!(
            fault_expectation(&json!({ "is": { "statusCode": 200 } })),
            None
        );
    }

    #[test]
    fn fault_expectation_certain_when_probability_absent() {
        // Rift's default probability is 1.0, so an absent `probability` means always-fires.
        let lat = json!({ "_rift": { "fault": { "latency": { "ms": 300 } } } });
        assert_eq!(
            fault_expectation(&lat),
            Some(FaultExpectation::Latency { ms: 300 })
        );
        let err = json!({ "_rift": { "fault": { "error": { "status": 500 } } } });
        assert_eq!(
            fault_expectation(&err),
            Some(FaultExpectation::ErrorStatus { status: 500 })
        );
    }

    #[test]
    fn is_dynamic_unassertable_flags_inject_without_verify() {
        assert!(is_dynamic_unassertable(&json!({
            "responses": [{ "inject": "function(){}" }]
        })));
        // A static `is` response is not this pass's concern (handled by the normal verifier).
        assert!(!is_dynamic_unassertable(&json!({
            "responses": [{ "is": { "statusCode": 200 } }]
        })));
        // A `_verify`-annotated stub is assertable, not a skip.
        assert!(!is_dynamic_unassertable(&json!({
            "responses": [{ "inject": "function(){}" }],
            "_verify": { "sequence": [] }
        })));
    }

    // ── AC4: latency threshold ─────────────────────────────────────────────
    #[test]
    fn latency_meets_threshold_is_at_least() {
        assert!(latency_meets_threshold(700, 700));
        assert!(latency_meets_threshold(950, 700));
        assert!(!latency_meets_threshold(699, 700));
    }

    // ── AC5: proxy record/prepend logic ────────────────────────────────────
    #[test]
    fn proxy_records_when_predicate_generators() {
        let with = json!({ "to": "http://x", "mode": "proxyOnce", "predicateGenerators": [{ "matches": { "path": true } }] });
        assert!(proxy_records_stub(&with));
        let always =
            json!({ "to": "http://x", "mode": "proxyAlways", "predicateGenerators": [{}] });
        assert!(proxy_records_stub(&always));
    }

    #[test]
    fn proxy_does_not_record_without_generators() {
        let without = json!({ "to": "http://x", "mode": "proxyOnce" });
        assert!(!proxy_records_stub(&without));
        let empty = json!({ "to": "http://x", "mode": "proxyAlways", "predicateGenerators": [] });
        assert!(!proxy_records_stub(&empty));
    }
}
