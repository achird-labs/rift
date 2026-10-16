//! Plane A conformance (issue #1346): the features in `conformance/features/rift/`
//! (`spec::PORTED_FEATURES`, every one since #1350), run against a real engine.
//!
//! - `RIFT_BUILD_DIR` (required): `rift`, `rift-lint`, `rift-verify` and `BUILD_INFO`, staged as
//!   `.github/workflows/conformance.yml` does. Unset, the run prints why and passes, unless
//!   `RIFT_CONFORMANCE_REQUIRE` is set (CI sets it), when it fails.
//! - `RIFT_MODE`: `external` (default; spawn the staged binary), `embedded` (link rift-ffi and serve
//!   the admin API in this process) or `container` (`docker run` the image `RIFT_IMAGE`, required,
//!   with the imposter ranges published 1:1; fresh imposters take ports from a pool there).
//! - `RIFT_CORPUS_ROOT`, `RIFT_FEATURES_DIR`, `RIFT_MAX_BODY_SIZE`: as for the Scala runner.
//!
//! cucumber's own flags still work after `--`: `--name <regex>`, `--tags <expr>`, `--fail-fast`.
//! They narrow the ported set; they never widen it past it or past what the mode can run.
//!
//! Step text is byte-identical to the retired Scala runner's (`RiftConformanceSupport.scala` and
//! the per-feature suites), so the feature files did not change in the move.

mod common;

use anyhow::{Context as _, anyhow, bail};
use cucumber::parser::{self, Parser};
use cucumber::tag::Ext as _;
use cucumber::writer::Stats as _;
use cucumber::{World as _, cli, gherkin, given, then, when};
use futures::FutureExt as _;
use regex::Regex;
use rift_conformance_runner::HarnessError;
use rift_conformance_runner::harness::{
    Config, EngineBoot, Harness, InterceptEngine, IsolatedEngine, ReloadEngine, SpawnedEngine,
    free_port, log_tail, port_taken,
};
use rift_conformance_runner::http::{
    Clients, FailureKind, Response, classify_failure, install_crypto_provider,
};
use rift_conformance_runner::prom::{self, PromText, Series};
use rift_conformance_runner::raw::{self, H2_INTERNAL_ERROR, H2Outcome, RawCapture};
use rift_conformance_runner::spec::{PORTED_FEATURES, features_dir, fixtures_dir};
use rift_conformance_runner::tools::{CORPUS_VERIFY_ARGS, binary_version};
use rift_conformance_runner::wire::{self, RawConnection, RawRead};
use rift_conformance_runner::{intercept, tls};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The steps' view of the harness `run()` owns. `run()` leaks it to get the `&'static` the step
/// functions need, and holds a [`Teardown`] guard so the engine is still stopped when the run
/// panics outside cucumber's per-step catch (a static is never dropped, so it cannot do that).
static HARNESS: OnceLock<&'static Harness> = OnceLock::new();
static CLIENTS: OnceLock<Clients> = OnceLock::new();
/// Scenarios that ran to their `after` hook, whatever their outcome.
static SCENARIOS_RUN: AtomicUsize = AtomicUsize::new(0);

fn harness() -> &'static Harness {
    HARNESS
        .get()
        .copied()
        .expect("main starts the harness before any scenario runs")
}

/// Kills the shared engine if `run()` unwinds before its orderly `Harness::shutdown`. After that
/// shutdown the engine slot is empty and this does nothing.
struct Teardown(&'static Harness);

impl Drop for Teardown {
    fn drop(&mut self) {
        self.0.kill_now();
    }
}

fn clients() -> &'static Clients {
    CLIENTS
        .get()
        .expect("main builds the clients before any scenario runs")
}

/// A step failure. cucumber prints a failing step's error with `{}`; this prints the whole anyhow
/// chain, so the cause is never cut off.
struct Failure(anyhow::Error);

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}

impl From<HarnessError> for Failure {
    fn from(e: HarnessError) -> Self {
        Self(e.into())
    }
}

type Step = Result<(), Failure>;

/// Request headers and body staged for the next send.
#[derive(Debug, Default)]
struct Pending {
    headers: Vec<(String, String)>,
    body: Option<String>,
}

/// Per-scenario state. A fresh one per scenario, as zio-bdd's `Stage` was.
#[derive(Debug, Default, cucumber::World)]
struct PlaneA {
    /// Imposters this scenario created on the shared engine, deleted after it.
    tracked: Vec<u16>,
    /// The imposter request steps target: the last one posted.
    current: Option<u16>,
    pending: Pending,
    last: Option<Response>,
    /// Paste-ready curl equivalents of this scenario's requests, appended to every failure.
    curl: Vec<String>,
    corpus: Option<Vec<PathBuf>>,
    posted_corpus: Option<Vec<u16>>,
    request_counter: Option<f64>,
    upstream_port: Option<u16>,
    /// A dedicated engine this scenario started (feature 09 `@upstream-trust`, features 05 and 19
    /// `@rift-debug-env`), killed after it. While one runs the flow-state steps address it.
    side_engine: Option<SpawnedEngine>,
    /// Feature 21's `@admin-isolated` engine, stopped after the scenario.
    isolated: Option<IsolatedEngine>,
    /// Imposter ports by the name a feature 21 step gave them, filled in as `{name}` in admin paths.
    named: BTreeMap<String, u16>,
    /// A response body kept for "differs from the remembered one" (feature 05).
    remembered: Option<String>,
    /// The last bare-socket capture (feature 03).
    raw: Option<RawCapture>,
    /// The two streams of the last h2c exchange (feature 03, rift#1234).
    h2c: Option<(H2Outcome, H2Outcome)>,
    /// Feature 11's engine serving a config file. While it runs it is the engine the admin steps
    /// address, as the Scala runner swapped its shared engine for one; stopped after the scenario.
    reload: Option<ReloadEngine>,
    /// How long the last request took, send to last body byte.
    elapsed: Option<Duration>,
    /// Whether the last "expecting a transport error" send failed at the transport level.
    transport_failed: Option<bool>,
    /// Each listener's exposition when the scenario snapshotted it (feature 17's deltas).
    snapshots: BTreeMap<String, PromText>,
    burst: Option<Burst>,
    /// The last feature 20 HTTPS exchange's chain and negotiated version.
    tls: Option<TlsExchange>,
    /// The last single-stream h2c GET (feature 20).
    h2c_get: Option<H2Outcome>,
    /// How the last boot-only engine ended (features 16 and 20).
    boot: Option<EngineBoot>,
    /// Whether this scenario added rules to the shared intercept listener, cleared after it.
    intercept_rules_added: bool,
    /// The open raw tunnel through an intercept listener, and the host it was opened to.
    tunnel: Option<(String, RawConnection)>,
    /// A plaintext connection to an intercept listener, kept to observe its close.
    listener_conn: Option<RawConnection>,
    /// The last response read off a raw tunnel or listener connection.
    raw_response: Option<wire::RawResponse>,
    /// Feature 12's `@intercept-runtime` engine (no intercept listener at start), stopped after it.
    runtime_engine: Option<IsolatedEngine>,
    /// The listener `POST /intercept` started on it.
    runtime_listener: Option<RuntimeListener>,
    /// Feature 12's `@intercept-auth` or `@intercept-ws` engine, stopped after the scenario.
    intercept_engine: Option<InterceptEngine>,
    /// Feature 12's WebSocket echo origin, stopped after the scenario.
    ws_origin: Option<intercept::WsEchoOrigin>,
}

/// A concurrent burst left running, joined by the step that asserts on it.
#[derive(Debug)]
struct Burst {
    requested: usize,
    requests: Vec<tokio::task::JoinHandle<Result<u16, String>>>,
}

impl PlaneA {
    fn current(&self) -> anyhow::Result<u16> {
        self.current
            .ok_or_else(|| anyhow!("no imposter posted yet in this scenario"))
    }

    fn last(&self) -> anyhow::Result<&Response> {
        self.last
            .as_ref()
            .ok_or_else(|| anyhow!("no response: send a request first"))
    }

    /// Assert, appending the scenario's curl repro so triage starts from a reproduction.
    fn ensure(&self, cond: bool, msg: impl FnOnce() -> String) -> Step {
        if cond {
            Ok(())
        } else {
            Err(anyhow!("{}\n--- repro ---\n{}", msg(), self.curl.join("\n")).into())
        }
    }

    fn record_curl(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) {
        let mut line = format!("curl -X {}", method.to_uppercase());
        for (k, v) in headers {
            line.push_str(&format!(" -H '{k}: {v}'"));
        }
        if let Some(b) = body {
            line.push_str(&format!(" -d '{b}'"));
        }
        line.push_str(&format!(" '{url}'"));
        self.curl.push(line);
    }

    /// Send the staged request with `client` and keep the response.
    async fn send(&mut self, client: &reqwest::Client, method: &str, url: &str) -> Step {
        let headers = self.pending.headers.clone();
        let body = self.pending.body.clone();
        self.send_with(client, method, url, &headers, body.as_deref())
            .await
    }

    /// Send exactly `headers` and `body` (not the staged request) and keep the response.
    async fn send_with(
        &mut self,
        client: &reqwest::Client,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Step {
        self.record_curl(method, url, headers, body);
        let (resp, took) = exchange(client, method, url, headers, body)
            .await?
            .with_context(|| format!("transport failure on {method} {url}"))?;
        self.last = Some(resp);
        self.elapsed = Some(took);
        Ok(())
    }

    /// The admin API the scenario's engine serves: the reload engine's while one runs.
    fn admin_url(&self) -> String {
        self.reload
            .as_ref()
            .map_or_else(|| harness().admin_url.clone(), |r| r.admin_url.clone())
    }

    /// The last response's body as JSON.
    fn last_json(&self) -> anyhow::Result<Value> {
        let body = &self.last()?.body;
        serde_json::from_str(body).with_context(|| format!("invalid admin JSON (body={body})"))
    }

    /// Assert `ok` holds for the value at `path` in the last response's JSON.
    fn ensure_json_at(&self, path: &str, what: &str, ok: impl Fn(&Value) -> bool) -> Step {
        let root = self.last_json()?;
        let v = json_at(&root, path);
        self.ensure(v.is_some_and(ok), || {
            format!(
                "expected {what} at '{path}', got {} in: {root}",
                v.map_or_else(|| "<absent>".to_string(), Value::to_string)
            )
        })
    }

    /// Send with `client`, expecting no HTTP response at all (a refused handshake).
    async fn expect_refused(&mut self, client: &reqwest::Client, url: &str, what: &str) -> Step {
        self.curl.push(format!(
            "curl -k '{url}'   # {what}; expected to be REFUSED"
        ));
        // Only a TLS-layer failure (or a timeout) counts as refused. Connection refused or a DNS
        // failure means a dead engine or an unbound imposter, which must fail the step, not pass
        // it: the Scala runner rethrows ConnectException/UnknownHostException for the same reason.
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                self.ensure(false, || {
                    format!(
                        "expected {what} to be refused at the TLS layer, but {url} answered HTTP {status}"
                    )
                })
            }
            Err(e) => match classify_failure(&e) {
                FailureKind::TlsRefused(_) => Ok(()),
                FailureKind::Unreachable(cause) => self.ensure(false, || {
                    format!("{url} was never reached, so nothing was refused: {cause}")
                }),
                FailureKind::Other(cause) => self.ensure(false, || {
                    format!("{url} failed, but not at the TLS layer: {cause}")
                }),
            },
        }
    }
}

/// One request, timed from sending to the last body byte (the Scala runner timed its whole JDK
/// send). The outer error is a request that could not be built; the inner one a transport failure.
async fn exchange(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> anyhow::Result<Result<(Response, Duration), reqwest::Error>> {
    let method_value = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
        .with_context(|| format!("HTTP method {method:?}"))?;
    let mut request = client.request(method_value, url);
    for (k, v) in headers {
        request = request.header(k, v);
    }
    if let Some(body) = body {
        request = request.body(body.to_string());
    }
    let started = Instant::now();
    let sent = match request.send().await {
        Ok(resp) => Response::read(resp).await,
        Err(e) => Err(e),
    };
    Ok(sent.map(|r| (r, started.elapsed())))
}

/// The value at a dotted path (`stubs.1.id`): each segment an object key or an array index.
fn json_at<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(root, |v, seg| match v {
        Value::Object(m) => m.get(seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Numeric equality across JSON's integer and float forms, as the Scala runner's `BigDecimal` one.
fn is_number(v: &Value, n: i64) -> bool {
    match v {
        Value::Number(x) => x
            .as_i64()
            .map_or_else(|| x.as_f64() == Some(n as f64), |i| i == n),
        _ => false,
    }
}

// ── fixtures and the admin API ───────────────────────────────────────────────────────────────

fn load_fixture(name: &str) -> anyhow::Result<String> {
    let path = fixtures_dir().join(format!("{name}.json"));
    std::fs::read_to_string(&path).with_context(|| format!("fixture {}", path.display()))
}

fn parse_object(raw: &str, what: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(raw).with_context(|| format!("{what} is not JSON"))? {
        Value::Object(map) => Ok(map),
        _ => bail!("{what} is not a JSON object"),
    }
}

fn declared_port(raw: &str, what: &str) -> anyhow::Result<u16> {
    let map = parse_object(raw, what)?;
    let port = map
        .get("port")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("{what} has no top-level port"))?;
    u16::try_from(port).with_context(|| format!("{what} port {port}"))
}

/// POST imposter JSON to `admin`, returning the status and body.
async fn post_imposter(admin: &str, json: &str) -> anyhow::Result<(u16, String)> {
    let resp = clients()
        .verifying
        .post(format!("{admin}/imposters"))
        .header("Content-Type", "application/json")
        .body(json.to_string())
        .send()
        .await
        .with_context(|| format!("POST {admin}/imposters"))?;
    let resp = Response::read(resp).await?;
    Ok((resp.status, resp.body))
}

/// POST an imposter with any `port` removed, so the engine assigns one, and return that port
/// (read back from the 201). No probe-then-bind window: the engine binds the port it reports. In
/// container mode the port comes from the harness's pool instead, walking on past a refused one.
async fn post_at_engine_port(admin: &str, raw: &str, what: &str) -> anyhow::Result<u16> {
    let mut map = parse_object(raw, what)?;
    map.remove("port");
    let body = if let Some(pool) = harness().imposter_port_pool() {
        let (_, body) = pool
            .claim(|port| {
                let mut map = map.clone();
                map.insert("port".to_string(), port.into());
                async move { post_imposter(admin, &Value::Object(map).to_string()).await }
            })
            .await
            .with_context(|| format!("POST imposter ({what})"))?;
        body
    } else {
        let (status, body) = post_imposter(admin, &Value::Object(map).to_string()).await?;
        if status != 201 {
            bail!("POST imposter ({what}) returned {status}, expected 201: {body}");
        }
        body
    };
    declared_port(&body, &format!("the 201 for {what}"))
}

async fn delete_imposter(admin: &str, port: u16) -> anyhow::Result<()> {
    let resp = clients()
        .verifying
        .delete(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .with_context(|| format!("DELETE {admin}/imposters/{port}"))?;
    let status = resp.status().as_u16();
    // 404: already gone, which is the goal.
    if status >= 300 && status != 404 {
        bail!("DELETE {admin}/imposters/{port} -> {status}");
    }
    Ok(())
}

/// zio-bdd's `string`: the rest of the step, trimmed, unquoted when it is quoted. Only an outline
/// step whose cell may be unquoted (`When the request body is <body>`) needs it; every other step
/// takes a strictly quoted argument.
fn flexible_string(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        unescape(&t[1..t.len() - 1])
    } else {
        t.to_string()
    }
}

/// Cucumber unescaping of a quoted argument: only `\"` and `\\` are escapes.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&n) = chars.peek()
            && (n == '"' || n == '\\')
        {
            out.push(n);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

fn sum_requests_total(text: &str) -> anyhow::Result<f64> {
    // `rift_requests_total{...}` or bare `rift_requests_total `, never a `_created` sibling. The
    // counter is registered lazily, so an engine that has served nothing has no line: that is 0.
    text.lines()
        .filter(|l| l.starts_with("rift_requests_total{") || l.starts_with("rift_requests_total "))
        .map(|l| {
            l.split_whitespace()
                .last()
                .unwrap_or_default()
                .parse::<f64>()
                .with_context(|| format!("unparseable rift_requests_total line {l:?}"))
        })
        .sum()
}

async fn requests_total(w: &PlaneA) -> anyhow::Result<f64> {
    let url = format!("{}/metrics", harness().metrics_url);
    let resp = Response::read(
        clients()
            .verifying
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?,
    )
    .await?;
    if resp.status != 200 {
        bail!(
            "GET {url} -> {} (expected 200)\n--- repro ---\n{}",
            resp.status,
            w.curl.join("\n")
        );
    }
    sum_requests_total(&resp.body)
}

// ── imposters ────────────────────────────────────────────────────────────────────────────────

#[given(regex = r#"^a fresh imposter from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn fresh_imposter(w: &mut PlaneA, name: String) -> Step {
    let h = harness();
    let raw = h.rewrite_fixture(&load_fixture(&name)?);
    let port = post_at_engine_port(&h.admin_url, &raw, &format!("fixture {name}")).await?;
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

/// Keeps the fixture's declared port: for imposters another one addresses by number
/// (`defaultForward`) or a step names by port.
#[given(regex = r#"^a fresh verbatim imposter from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn fresh_verbatim_imposter(w: &mut PlaneA, name: String) -> Step {
    let h = harness();
    let raw = h.rewrite_fixture(&load_fixture(&name)?);
    let port = declared_port(&raw, &format!("fixture {name}"))?;
    let (status, body) = post_imposter(&h.admin_url, &raw).await?;
    if status != 201 {
        return Err(anyhow!(
            "POST verbatim fixture {name} returned {status}, expected 201: {body}"
        )
        .into());
    }
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

// ── requests ─────────────────────────────────────────────────────────────────────────────────

// Also a `Given`: feature 19's Background stages its flow header with `And` after a `Given`.
#[given(regex = r#"^the request header "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#)]
#[when(regex = r#"^the request header "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#)]
async fn request_header(w: &mut PlaneA, name: String, value: String) {
    // Staging a header again replaces it, as the Scala runner's header map did.
    let name = unescape(&name);
    w.pending.headers.retain(|(k, _)| *k != name);
    w.pending.headers.push((name, unescape(&value)));
}

// Anchored to the forms the features use: a quoted string, a bare outline cell, or an empty one.
// A bare cell may not start with a digit, which keeps "the request body is <n> MiB of …"
// (features 13 and 21) from matching this step too.
#[when(regex = r#"^the request body is(?: ("(?:\\.|[^"\\])*"|[^"\s\d].*))? ?$"#)]
async fn request_body(w: &mut PlaneA, body: String) {
    w.pending.body = Some(flexible_string(&body));
}

#[when(regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)"$"#)]
async fn send_to_current(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let url = format!(
        "http://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.send(&clients().verifying, &method, &url).await
}

/// Through the single-port gateway: `{admin}/__rift/<port><path>`.
#[when(regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" via the gateway$"#)]
async fn send_via_gateway(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let url = format!("{}/__rift/{port}{}", harness().admin_url, unescape(&path));
    w.send(&clients().verifying, &method, &url).await
}

#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" over HTTPS to port (\d+)$"#
)]
async fn send_https_to_port(w: &mut PlaneA, method: String, path: String, port: u16) -> Step {
    let url = format!(
        "https://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.send(&clients().insecure, &method, &url).await
}

#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" over HTTPS presenting client certificate "((?:\\.|[^"\\])*)"$"#
)]
async fn send_mtls(w: &mut PlaneA, method: String, path: String, cert: String) -> Step {
    let port = w.current()?;
    let client = Clients::mtls(&fixtures_dir(), &unescape(&cert))?;
    let url = format!(
        "https://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.send(&client, &method, &url).await
}

// ── responses ────────────────────────────────────────────────────────────────────────────────

#[then(regex = r"^the response status is (\d+)$")]
async fn response_status(w: &mut PlaneA, code: u16) -> Step {
    let got = w.last()?.status;
    w.ensure(got == code, || format!("expected status {code}, got {got}"))
}

#[then(regex = r#"^the response body is "((?:\\.|[^"\\])*)"$"#)]
async fn response_body_is(w: &mut PlaneA, body: String) -> Step {
    let want = unescape(&body);
    let got = &w.last()?.body;
    w.ensure(*got == want, || {
        format!("expected body '{want}', got '{got}'")
    })
}

#[then(regex = r#"^the response body contains "((?:\\.|[^"\\])*)"$"#)]
async fn response_body_contains(w: &mut PlaneA, sub: String) -> Step {
    let want = unescape(&sub);
    let got = &w.last()?.body;
    w.ensure(got.contains(&want), || {
        format!("expected body to contain '{want}', got '{got}'")
    })
}

#[then(regex = r#"^the response body matches "((?:\\.|[^"\\])*)"$"#)]
async fn response_body_matches(w: &mut PlaneA, pattern: String) -> Step {
    let pattern = unescape(&pattern);
    let re = Regex::new(&pattern).with_context(|| format!("bad regex /{pattern}/"))?;
    let got = &w.last()?.body;
    w.ensure(re.is_match(got), || {
        format!("body '{got}' did not match /{pattern}/")
    })
}

#[then(regex = r#"^the response has (\d+) "((?:\\.|[^"\\])*)" headers$"#)]
async fn response_header_count(w: &mut PlaneA, n: usize, name: String) -> Step {
    let name = unescape(&name);
    let got = w.last()?.header_values(&name).len();
    w.ensure(got == n, || {
        format!("expected {n} '{name}' header(s), got {got}")
    })
}

#[then(regex = r#"^the response header "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#)]
async fn response_header_is(w: &mut PlaneA, name: String, value: String) -> Step {
    let (name, want) = (unescape(&name), unescape(&value));
    // Several values compare as one comma-joined value, as the Scala runner's did.
    let values = w.last()?.header_values(&name);
    let got = (!values.is_empty()).then(|| values.join(","));
    w.ensure(got.as_deref() == Some(want.as_str()), || {
        format!(
            "expected header {name}={want}, got {}",
            got.as_deref().unwrap_or("<absent>")
        )
    })
}

// ── TLS (feature 09): self-signed HTTPS, mutualAuth (rift#977), --upstream-ca-file (rift#974) ──

/// A client verifying the chain against the platform trust store must reject the imposter's
/// self-signed certificate: that, not merely "some TLS was spoken", is what proves it self-signed.
#[then(
    regex = r#"^a cert-verifying HTTPS request to port (\d+) at "((?:\\.|[^"\\])*)" is rejected$"#
)]
async fn verifying_https_rejected(w: &mut PlaneA, port: u16, path: String) -> Step {
    let url = format!(
        "https://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.expect_refused(&clients().verifying, &url, "a cert-verifying client")
        .await
}

#[then(
    regex = r#"^an HTTPS request to "((?:\\.|[^"\\])*)" presenting no client certificate is refused$"#
)]
async fn mtls_no_cert_refused(w: &mut PlaneA, path: String) -> Step {
    let port = w.current()?;
    let url = format!(
        "https://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.expect_refused(
        &clients().insecure,
        &url,
        "a client presenting no certificate",
    )
    .await
}

#[then(
    regex = r#"^an HTTPS request to "((?:\\.|[^"\\])*)" presenting client certificate "((?:\\.|[^"\\])*)" is refused$"#
)]
async fn mtls_cert_refused(w: &mut PlaneA, path: String, cert: String) -> Step {
    let port = w.current()?;
    let cert = unescape(&cert);
    let client = Clients::mtls(&fixtures_dir(), &cert)?;
    let url = format!(
        "https://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.expect_refused(
        &client,
        &url,
        &format!("a client presenting certificate {cert}"),
    )
    .await
}

#[given(regex = r#"^an HTTPS upstream imposter from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn https_upstream(w: &mut PlaneA, name: String) -> Step {
    fresh_imposter(w, name).await?;
    w.upstream_port = w.current;
    Ok(())
}

fn proxy_to_upstream_json(w: &PlaneA) -> anyhow::Result<String> {
    let port = w
        .upstream_port
        .ok_or_else(|| anyhow!("no HTTPS upstream imposter in this scenario"))?;
    // `localhost`: the upstream's leaf certificate is issued for it.
    Ok(serde_json::json!({
        "protocol": "http",
        "stubs": [{"responses": [{"proxy": {"to": format!("https://localhost:{port}"), "mode": "proxyAlways"}}]}]
    })
    .to_string())
}

#[when(regex = r"^a proxy imposter to the HTTPS upstream is created$")]
async fn proxy_to_upstream(w: &mut PlaneA) -> Step {
    let json = proxy_to_upstream_json(w)?;
    let port =
        post_at_engine_port(&harness().admin_url, &json, "proxy to the HTTPS upstream").await?;
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

/// On a dedicated engine started with `--upstream-ca-file` = the committed test CA. The imposter
/// is not tracked: the whole engine is killed after the scenario.
#[when(
    regex = r"^a proxy imposter to the HTTPS upstream is created on an engine trusting the test CA$"
)]
async fn proxy_to_upstream_trusting(w: &mut PlaneA) -> Step {
    let json = proxy_to_upstream_json(w)?;
    if let Some(old) = w.side_engine.take() {
        old.stop().await?;
    }
    let ca = fixtures_dir().join("intercept").join("ca-cert.pem");
    let engine = harness().spawn_upstream_trust_engine(&ca).await?;
    let admin = engine.admin_url.clone();
    w.side_engine = Some(engine);
    let port = post_at_engine_port(
        &admin,
        &json,
        "proxy to the HTTPS upstream (upstream-trust engine)",
    )
    .await?;
    w.current = Some(port);
    Ok(())
}

// ── metrics (feature 09) ─────────────────────────────────────────────────────────────────────

/// The counter is cumulative across the shared engine, so only a delta around the scenario's own
/// requests means anything.
#[when(regex = r"^the client records the request counter$")]
async fn record_counter(w: &mut PlaneA) -> Step {
    w.request_counter = Some(requests_total(w).await?);
    Ok(())
}

#[then(regex = r"^the request counter increased by at least (\d+)$")]
async fn counter_increased(w: &mut PlaneA, n: u32) -> Step {
    let before = w
        .request_counter
        .ok_or_else(|| anyhow!("no recorded request counter"))?;
    let after = requests_total(w).await?;
    let delta = after - before;
    w.ensure(delta >= f64::from(n), || {
        format!("expected the request counter to increase by >= {n}, got delta={delta} (before={before}, after={after})")
    })
}

#[then(regex = r"^the metrics endpoint reports at least (\d+) requests$")]
async fn metrics_at_least(w: &mut PlaneA, n: u32) -> Step {
    let total = requests_total(w).await?;
    w.ensure(total >= f64::from(n), || {
        format!("expected metrics rift_requests_total sum >= {n}, got {total}")
    })
}

// ── consistency gate (feature 01) ────────────────────────────────────────────────────────────

const BINARIES: &[&str] = &["rift", "rift-verify", "rift-lint"];

#[then(regex = r"^BUILD_INFO records a released build and all three binaries$")]
async fn build_info_complete(w: &mut PlaneA) -> Step {
    let info = &harness().build_info;
    let missing: Vec<&str> = BINARIES
        .iter()
        .copied()
        .filter(|b| !info.versions.contains_key(*b))
        .collect();
    w.ensure(
        !info.commit.is_empty() && !info.branch.is_empty() && missing.is_empty(),
        || {
            format!(
                "BUILD_INFO coherence: commit='{}' branch='{}' missing={}",
                info.commit,
                info.branch,
                missing.join(",")
            )
        },
    )
}

/// Each binary must exist in the build dir and its live `--version` must match what BUILD_INFO
/// recorded: a missing, swapped or stale binary fails here.
#[then(regex = r"^the engine, rift-lint and rift-verify binaries match BUILD_INFO$")]
async fn binaries_match_build_info(w: &mut PlaneA) -> Step {
    let h = harness();
    let mut problems = Vec::new();
    for bin in BINARIES {
        let path = h.config.build_dir.join(bin);
        if !path.is_file() {
            problems.push(format!("missing binary: {bin}"));
            continue;
        }
        let live = binary_version(&path).await?;
        let recorded = h
            .build_info
            .versions
            .get(*bin)
            .map_or("<none>", String::as_str);
        if live != recorded {
            problems.push(format!(
                "{bin} drift: on-disk '{live}' != BUILD_INFO '{recorded}'"
            ));
        }
    }
    w.ensure(problems.is_empty(), || {
        format!("binary/BUILD_INFO coherence: {}", problems.join("; "))
    })
}

fn corpus_imposters_dir() -> PathBuf {
    harness().config.corpus_root.join("imposters")
}

/// The corpus `.json` files, sorted (add-never-renumber keeps the order stable).
fn corpus_files() -> anyhow::Result<Vec<PathBuf>> {
    let dir = corpus_imposters_dir();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("corpus dir {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|e| e == "json") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[given(regex = r"^the corpus directory is loaded$")]
async fn corpus_loaded(w: &mut PlaneA) -> Step {
    w.corpus = Some(corpus_files()?);
    Ok(())
}

#[then(regex = r"^the corpus contains at least (\d+) imposter files$")]
async fn corpus_size(w: &mut PlaneA, min: usize) -> Step {
    // A Then without its Given must fail, never pass vacuously on an empty default.
    let n = w
        .corpus
        .as_ref()
        .ok_or_else(|| anyhow!("corpus not loaded"))?
        .len();
    w.ensure(n >= min, || {
        format!("corpus has {n} imposter files, expected >= {min}")
    })
}

#[then(regex = r"^rift-lint reports 0 errors across the corpus$")]
async fn lint_corpus(w: &mut PlaneA) -> Step {
    let report = harness().tools.lint(&corpus_imposters_dir()).await?;
    // A misdirected or empty lint run would report a trivial "0 errors".
    w.ensure(report.files_checked >= 15, || {
        format!(
            "rift-lint only checked {} files (expected >= 15: wrong corpus dir?)",
            report.files_checked
        )
    })?;
    w.ensure(report.errors == 0, || {
        format!(
            "rift-lint reported {} error(s): {}",
            report.errors,
            report.error_offenders().join("; ")
        )
    })
}

/// Every corpus file POSTed verbatim: declared 45xx ports and `_verify` blocks intact, so the
/// corpus stays an independent oracle. Fails naming every file the engine rejected.
#[given(regex = r"^every corpus imposter is posted to the engine$")]
async fn post_corpus(w: &mut PlaneA) -> Step {
    let h = harness();
    let files = corpus_files()?;
    if files.is_empty() {
        return Err(anyhow!(
            "no corpus imposters under {}",
            corpus_imposters_dir().display()
        )
        .into());
    }
    let mut ports = Vec::new();
    let mut rejected = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let raw = std::fs::read_to_string(&file).with_context(|| format!("reading {name}"))?;
        let raw = h.rewrite_fixture(&raw);
        let port = declared_port(&raw, &name)?;
        let (status, body) = post_imposter(&h.admin_url, &raw).await?;
        w.tracked.push(port);
        if status == 201 {
            ports.push(port);
        } else {
            rejected.push(format!("{name} ({status}: {body})"));
        }
    }
    if !rejected.is_empty() {
        return Err(anyhow!("engine rejected corpus files: {}", rejected.join(", ")).into());
    }
    w.posted_corpus = Some(ports);
    Ok(())
}

/// Protocol-agnostic "serves on its declared port": something accepts a TCP connection there.
#[then(regex = r"^every corpus imposter answers on its declared port$")]
async fn corpus_answers(w: &mut PlaneA) -> Step {
    let ports = w
        .posted_corpus
        .clone()
        .ok_or_else(|| anyhow!("corpus not posted"))?;
    let mut dead = Vec::new();
    for port in ports {
        let connect = tokio::net::TcpStream::connect((harness().imposter_host(), port));
        if !matches!(
            tokio::time::timeout(Duration::from_secs(1), connect).await,
            Ok(Ok(_))
        ) {
            dead.push(port.to_string());
        }
    }
    w.ensure(dead.is_empty(), || {
        format!("imposter ports not answering: {}", dead.join(", "))
    })
}

#[then(regex = r"^rift-verify reports 0 failures$")]
async fn verify_corpus(w: &mut PlaneA) -> Step {
    let h = harness();
    let report = h.tools.verify(&h.admin_url, CORPUS_VERIFY_ARGS).await?;
    // Lead with which test failed and why: a bare count sends the reader to the CI log.
    let detail = if !report.fail_detail.is_empty() {
        format!("\n  {}", report.fail_detail.join("\n  "))
    } else if !report.raw_err.trim().is_empty() {
        let lines: Vec<&str> = report.raw_err.lines().collect();
        format!(
            "\n  (no FAIL lines matched; stderr tail)\n  {}",
            lines[lines.len().saturating_sub(40)..].join("\n  ")
        )
    } else {
        "\n  (rift-verify wrote nothing to stderr)".to_string()
    };
    w.ensure(report.failed == 0, || {
        format!(
            "rift-verify: {} failed, {} passed, {} skipped{detail}\n{}",
            report.failed, report.passed, report.skipped, report.raw
        )
    })
}

// ── more imposter setups ─────────────────────────────────────────────────────────────────────

/// A raw POST of a fixture expected to be refused at creation: the response is kept for the
/// status/body steps rather than asserted 201. It goes without a `port` (a container-pool one in
/// container mode), and whatever port a regression that accepts it is given is tracked, so the
/// stray imposter cannot squat it.
#[when(regex = r#"^the client posts the imposter fixture "((?:\\.|[^"\\])*)"$"#)]
async fn post_fixture_raw(w: &mut PlaneA, name: String) -> Step {
    let raw = harness().rewrite_fixture(&load_fixture(&name)?);
    let what = format!("fixture {name}");
    let mut map = parse_object(&raw, &what)?;
    map.remove("port");
    if let Some(pool) = harness().imposter_port_pool() {
        let port = pool.next_port();
        w.tracked.push(port);
        map.insert("port".to_string(), port.into());
    }
    let json = Value::Object(map).to_string();
    let url = format!("{}/imposters", w.admin_url());
    w.send_with(
        &clients().verifying,
        "POST",
        &url,
        &json_content_type(),
        Some(&json),
    )
    .await?;
    let resp = w.last()?;
    if resp.status == 201 {
        let port = declared_port(&resp.body, &format!("the 201 for {what}"))?;
        w.tracked.push(port);
    }
    Ok(())
}

fn json_content_type() -> Vec<(String, String)> {
    vec![("Content-Type".to_string(), "application/json".to_string())]
}

// Feature 14 (rift#1194): a plain-HTTP upstream in the same engine, for proxy responses.
#[given(regex = r#"^an upstream imposter from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn upstream_imposter(w: &mut PlaneA, name: String) -> Step {
    fresh_imposter(w, name).await?;
    w.upstream_port = w.current;
    Ok(())
}

fn upstream(w: &PlaneA) -> anyhow::Result<u16> {
    w.upstream_port
        .ok_or_else(|| anyhow!("no upstream imposter in this scenario"))
}

/// The fixture names its proxy target `http://localhost:UPSTREAM_PORT`; the placeholder becomes the
/// scenario's upstream port, known only once that imposter is posted.
#[given(regex = r#"^a fresh imposter from fixture "((?:\\.|[^"\\])*)" proxying to the upstream$"#)]
async fn fresh_proxy_imposter(w: &mut PlaneA, name: String) -> Step {
    let up = upstream(w)?;
    let h = harness();
    let raw = h
        .rewrite_fixture(&load_fixture(&name)?)
        .replace("UPSTREAM_PORT", &up.to_string());
    let port = post_at_engine_port(&h.admin_url, &raw, &format!("fixture {name}")).await?;
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

/// `GET {admin}/imposters/{port}`'s `numberOfRequests`.
async fn number_of_requests(w: &PlaneA, port: u16) -> anyhow::Result<i64> {
    let url = format!("{}/imposters/{port}", w.admin_url());
    let resp = get(&url).await?;
    if resp.status != 200 {
        bail!(
            "GET {url} -> {} (expected 200): {}\n--- repro ---\n{}",
            resp.status,
            resp.body,
            w.curl.join("\n")
        );
    }
    let json: Value = serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
    json.get("numberOfRequests")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("no 'numberOfRequests' field: {}", resp.body))
}

async fn get(url: &str) -> anyhow::Result<Response> {
    let resp = clients()
        .verifying
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    Response::read(resp)
        .await
        .with_context(|| format!("reading the response of GET {url}"))
}

#[then(regex = r"^the upstream imposter has recorded (\d+) requests$")]
async fn upstream_recorded(w: &mut PlaneA, n: i64) -> Step {
    let got = number_of_requests(w, upstream(w)?).await?;
    w.ensure(got == n, || {
        format!("expected the upstream to have received {n} request(s), got {got}")
    })
}

#[then(regex = r"^imposter (\d+) has recorded (\d+) requests$")]
async fn imposter_recorded(w: &mut PlaneA, port: u16, n: i64) -> Step {
    let got = number_of_requests(w, port).await?;
    w.ensure(got == n, || {
        format!("expected imposter {port} to have recorded {n} request(s), got {got}")
    })
}

// rift#1199: `GET /imposters/:port` writes a multi-item `_behaviors.copy` back as a `behaviors`
// array holding one `{"copy": {...}}` element per item, the shape Mountebank itself loads.
#[then(
    regex = r"^the current imposter writes stub (\d+) back with (\d+) single-item copy behaviors$"
)]
async fn stub_written_back_with_copies(w: &mut PlaneA, index: usize, n: usize) -> Step {
    let url = format!("{}/imposters/{}", w.admin_url(), w.current()?);
    let resp = get(&url).await?;
    let json: Value = serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
    let behaviors = json
        .get("stubs")
        .and_then(Value::as_array)
        .and_then(|stubs| stubs.get(index))
        .and_then(|stub| stub.get("responses"))
        .and_then(Value::as_array)
        .and_then(|responses| responses.first())
        .and_then(|first| first.get("behaviors"))
        .and_then(Value::as_array);
    let single_copies = behaviors.map_or(0, |b| {
        b.iter()
            .filter(|e| {
                e.as_object()
                    .is_some_and(|o| o.len() == 1 && o.get("copy").is_some_and(Value::is_object))
            })
            .count()
    });
    w.ensure(
        behaviors.is_some_and(|b| b.len() == n) && single_copies == n,
        || {
            format!(
                "expected stub {index} written back as {n} single-item copy behaviors, got {}",
                behaviors.map_or_else(
                    || "<no behaviors array>".to_string(),
                    |b| Value::Array(b.clone()).to_string()
                )
            )
        },
    )
}

// ── more requests ────────────────────────────────────────────────────────────────────────────

#[when(regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" to port (\d+)$"#)]
async fn send_to_port(w: &mut PlaneA, method: String, path: String, port: u16) -> Step {
    let url = format!(
        "http://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    w.send(&clients().verifying, &method, &url).await
}

/// A transport-level failure is recorded, not raised. A refused connection or an unresolved host is
/// still raised: that is an unreachable engine, never a fault (the Scala runner rethrows
/// ConnectException/UnknownHostException for the same reason).
#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" expecting a transport error$"#
)]
async fn send_expecting_transport_error(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let url = format!(
        "http://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    let headers = w.pending.headers.clone();
    let body = w.pending.body.clone();
    w.record_curl(&method, &url, &headers, body.as_deref());
    let started = Instant::now();
    match exchange(
        &clients().verifying,
        &method,
        &url,
        &headers,
        body.as_deref(),
    )
    .await?
    {
        Ok((resp, took)) => {
            w.last = Some(resp);
            w.elapsed = Some(took);
            w.transport_failed = Some(false);
        }
        Err(e) => {
            if let FailureKind::Unreachable(cause) = classify_failure(&e) {
                return Err(anyhow!("{url} was never reached: {cause}").into());
            }
            if e.is_builder() {
                return Err(anyhow::Error::new(e)
                    .context(format!("building {method} {url}"))
                    .into());
            }
            // Stricter than the Scala runner, which counted any failure: an engine that accepts
            // and then hangs must not pass as a transport fault.
            if e.is_timeout() {
                return Err(anyhow::Error::new(e)
                    .context(format!(
                        "{method} {url} timed out instead of failing at the transport level"
                    ))
                    .into());
            }
            w.elapsed = Some(started.elapsed());
            w.transport_failed = Some(true);
        }
    }
    Ok(())
}

#[then(regex = r"^the request fails at the transport level$")]
async fn request_failed_at_transport(w: &mut PlaneA) -> Step {
    let failed = w
        .transport_failed
        .ok_or_else(|| anyhow!("no transport probe: send expecting a transport error first"))?;
    let got = w.last.as_ref().map(|r| r.status);
    w.ensure(failed, || {
        format!(
            "expected a transport-level failure{}",
            got.map_or_else(String::new, |s| format!(", but got HTTP {s}"))
        )
    })
}

#[when(regex = r#"^the request header "((?:\\.|[^"\\])*)" is removed$"#)]
async fn request_header_removed(w: &mut PlaneA, name: String) {
    let name = unescape(&name);
    w.pending
        .headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
}

/// An admin request; `payload` is `-` for none, else a fixture name. In the path and the payload
/// alike, `{port}` becomes the current imposter's port, `{unknownPort}` a port with no imposter on
/// it, `{newPort}` a port for a create (feature 13) and `{name}` the port of the imposter a step
/// named `name` (feature 21).
#[when(
    regex = r#"^the client sends admin "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" with payload "((?:\\.|[^"\\])*)"$"#
)]
async fn send_admin(w: &mut PlaneA, method: String, path: String, payload: String) -> Step {
    let admin = w.admin_url();
    send_admin_to(w, &admin, &method, &path, &payload).await
}

/// Feature 21 `@admin-isolated`: the same request against the scenario's isolated engine.
#[when(
    regex = r#"^the client sends isolated admin "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" with payload "((?:\\.|[^"\\])*)"$"#
)]
async fn send_isolated_admin(
    w: &mut PlaneA,
    method: String,
    path: String,
    payload: String,
) -> Step {
    let admin = w
        .isolated
        .as_ref()
        .map(|e| e.admin_url.clone())
        .ok_or_else(|| anyhow!("no isolated engine started in this scenario"))?;
    send_admin_to(w, &admin, &method, &path, &payload).await
}

/// A port no imposter of `admin`'s engine is on. In container mode it is a published pool port, so
/// the request reaches the engine either way; elsewhere a port the OS has free. It is tracked: if
/// a door that should refuse starts creating, the stray imposter is still torn down.
async fn unknown_port(w: &mut PlaneA, admin: &str) -> anyhow::Result<u16> {
    let listed = get(&format!("{admin}/imposters")).await?;
    let json: Value = serde_json::from_str(&listed.body)
        .with_context(|| format!("invalid admin JSON (body={})", listed.body))?;
    let taken: Vec<u64> = json
        .get("imposters")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no 'imposters' array in: {}", listed.body))?
        .iter()
        .filter_map(|i| i.get("port").and_then(Value::as_u64))
        .collect();
    for _ in 0..32 {
        let port = match harness().imposter_port_pool() {
            Some(pool) => pool.next_port(),
            None => free_port()?,
        };
        if !taken.contains(&u64::from(port)) {
            w.tracked.push(port);
            return Ok(port);
        }
    }
    bail!("found no port without an imposter in 32 tries")
}

async fn send_admin_to(
    w: &mut PlaneA,
    admin: &str,
    method: &str,
    path: &str,
    payload: &str,
) -> Step {
    let (path, payload) = (unescape(path), unescape(payload));
    // A rule installed through this step lands on the shared listener, whose rules outlive the
    // scenario: mark it for the clear after it before the request goes out, so a refused-by-mistake
    // or leaked rule cannot reach a later scenario that lists or counts them.
    if matches!(method, "POST" | "PUT") && path.starts_with("/intercept/rules") {
        w.intercept_rules_added = true;
    }
    let body = if payload == "-" {
        None
    } else {
        Some(load_fixture(&payload)?)
    };
    let mentions = |p: &str| path.contains(p) || body.as_ref().is_some_and(|b| b.contains(p));
    let mut fills: Vec<(String, u16)> = Vec::new();
    if mentions("{port}") {
        fills.push(("{port}".to_string(), w.current()?));
    }
    if mentions("{unknownPort}") {
        fills.push(("{unknownPort}".to_string(), unknown_port(w, admin).await?));
    }
    // `0` has the engine assign the port, so nothing probes for a free one (#1461); a container
    // engine's would be unreachable, so there it is a pool port. The doors that use it refuse the
    // create; a regression that accepts it has its port tracked below.
    let new_port = mentions("{newPort}");
    if new_port {
        let port = match harness().imposter_port_pool() {
            Some(pool) => {
                let port = pool.next_port();
                w.tracked.push(port);
                port
            }
            None => 0,
        };
        fills.push(("{newPort}".to_string(), port));
    }
    fills.extend(w.named.iter().map(|(n, p)| (format!("{{{n}}}"), *p)));
    let fill = |s: &str| {
        fills
            .iter()
            .fold(s.to_string(), |acc, (k, p)| acc.replace(k, &p.to_string()))
    };
    let url = format!("{admin}{}", fill(&path));
    let body = body.map(|b| fill(&b));
    // A placeholder this runner does not fill (`{newPort}`, a name no step gave) would otherwise
    // go out literally, and a 4xx expectation would pass for the wrong reason.
    let unfilled = Regex::new(r"\{[A-Za-z0-9]+\}").expect("static regex");
    if let Some(m) = std::iter::once(url.as_str())
        .chain(body.as_deref())
        .find_map(|s| unfilled.find(s))
    {
        return Err(anyhow!(
            "admin payload placeholder {} is not supported by this runner",
            m.as_str()
        )
        .into());
    }
    w.send_with(
        &clients().verifying,
        method,
        &url,
        &json_content_type(),
        body.as_deref(),
    )
    .await?;
    let resp = w.last()?;
    if new_port && resp.status == 201 {
        let port = declared_port(&resp.body, "the 201 for a {newPort} create")?;
        w.tracked.push(port);
        w.current = Some(port);
    }
    Ok(())
}

// ── more responses ───────────────────────────────────────────────────────────────────────────

#[then(regex = r#"^the response has no header "((?:\\.|[^"\\])*)"$"#)]
async fn response_has_no_header(w: &mut PlaneA, name: String) -> Step {
    let name = unescape(&name);
    let values = w.last()?.header_values(&name);
    let got = values.join(",");
    w.ensure(values.is_empty(), || {
        format!("expected no header {name}, got {got}")
    })
}

#[then(regex = r"^the response took at least (\d+) ms$")]
async fn response_took_at_least(w: &mut PlaneA, floor: u64) -> Step {
    let took = w
        .elapsed
        .ok_or_else(|| anyhow!("no timing: send a request first"))?;
    let ms = took.as_millis();
    w.ensure(ms >= u128::from(floor), || {
        format!("expected the request to take >= {floor} ms, took {ms} ms")
    })
}

#[then(regex = r#"^the response JSON has no top-level field "((?:\\.|[^"\\])*)"$"#)]
async fn json_no_top_level_field(w: &mut PlaneA, field: String) -> Step {
    let field = unescape(&field);
    let Value::Object(map) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON object: {}", w.last()?.body).into());
    };
    w.ensure(!map.contains_key(&field), || {
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        format!(
            "expected no top-level field '{field}', got {}",
            keys.join(", ")
        )
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is the string "((?:\\.|[^"\\])*)"$"#)]
async fn json_at_string(w: &mut PlaneA, path: String, want: String) -> Step {
    let want = unescape(&want);
    w.ensure_json_at(&unescape(&path), &format!("the string \"{want}\""), |v| {
        v.as_str() == Some(want.as_str())
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is the number (\d+)$"#)]
async fn json_at_number(w: &mut PlaneA, path: String, n: i64) -> Step {
    w.ensure_json_at(&unescape(&path), &format!("the number {n}"), |v| {
        is_number(v, n)
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is the boolean (true|false)$"#)]
async fn json_at_bool(w: &mut PlaneA, path: String, want: String) -> Step {
    let want = want == "true";
    w.ensure_json_at(&unescape(&path), &want.to_string(), |v| {
        v.as_bool() == Some(want)
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is an empty array$"#)]
async fn json_at_empty_array(w: &mut PlaneA, path: String) -> Step {
    w.ensure_json_at(&unescape(&path), "an empty array", |v| {
        v.as_array().is_some_and(Vec::is_empty)
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" has (\d+) elements$"#)]
async fn json_at_len(w: &mut PlaneA, path: String, n: usize) -> Step {
    w.ensure_json_at(&unescape(&path), &format!("an array of {n}"), |v| {
        v.as_array().is_some_and(|a| a.len() == n)
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" contains "((?:\\.|[^"\\])*)"$"#)]
async fn json_at_contains(w: &mut PlaneA, path: String, sub: String) -> Step {
    let sub = unescape(&sub);
    w.ensure_json_at(
        &unescape(&path),
        &format!("a string containing \"{sub}\""),
        |v| v.as_str().is_some_and(|s| s.contains(&sub)),
    )
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is absent$"#)]
async fn json_at_absent(w: &mut PlaneA, path: String) -> Step {
    let path = unescape(&path);
    let root = w.last_json()?;
    let v = json_at(&root, &path);
    w.ensure(v.is_none(), || {
        format!(
            "expected nothing at '{path}', got {} in: {root}",
            v.map_or_else(String::new, Value::to_string)
        )
    })
}

#[then(regex = r"^the response is a JSON array of (\d+) elements$")]
async fn response_json_array_len(w: &mut PlaneA, n: usize) -> Step {
    let Value::Array(es) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON array: {}", w.last()?.body).into());
    };
    w.ensure(es.len() == n, || {
        format!(
            "expected {n} elements, got {}: {}",
            es.len(),
            Value::Array(es.clone())
        )
    })
}

/// `k=v,k2=v2`: each value is `absent`, an integer, or a string, the exact field a warning carries.
fn warning_matches(warning: &serde_json::Map<String, Value>, fields: &str) -> bool {
    fields
        .split(',')
        .map(str::trim)
        .filter(|kv| !kv.is_empty())
        .all(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let got = warning.get(k);
            match v {
                "absent" => got.is_none(),
                n if n.parse::<i32>().is_ok() => {
                    got.is_some_and(|g| n.parse::<i64>().is_ok_and(|n| is_number(g, n)))
                }
                s => got.and_then(Value::as_str) == Some(s),
            }
        })
}

#[then(
    regex = r#"^the response has a "((?:\\.|[^"\\])*)" warning with "((?:\\.|[^"\\])*)" and a message containing "((?:\\.|[^"\\])*)"$"#
)]
async fn response_has_warning(w: &mut PlaneA, kind: String, fields: String, sub: String) -> Step {
    let (kind, fields, sub) = (unescape(&kind), unescape(&fields), unescape(&sub));
    let root = w.last_json()?;
    let warnings: Vec<&serde_json::Map<String, Value>> = json_at(&root, "_rift.warnings")
        .and_then(Value::as_array)
        .map(|ws| ws.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    let hit = warnings.iter().any(|wn| {
        wn.get("warningType").and_then(Value::as_str) == Some(kind.as_str())
            && warning_matches(wn, &fields)
            && wn
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|m| m.contains(&sub))
    });
    w.ensure(hit, || {
        let listed: Vec<String> = warnings
            .iter()
            .map(|wn| Value::Object((*wn).clone()).to_string())
            .collect();
        format!(
            "expected a '{kind}' warning with {fields} and a message containing '{sub}'; got [{}]",
            listed.join(", ")
        )
    })
}

// ── rift-lint on a fixture (feature 14) ──────────────────────────────────────────────────────

async fn lint_fixture(name: &str) -> anyhow::Result<rift_conformance_runner::tools::LintReport> {
    let path = fixtures_dir().join(format!("{name}.json"));
    Ok(harness().tools.lint(&path).await?)
}

#[then(regex = r#"^rift-lint reports "((?:\\.|[^"\\])*)" for fixture "((?:\\.|[^"\\])*)"$"#)]
async fn lint_reports(w: &mut PlaneA, code: String, name: String) -> Step {
    let (code, name) = (unescape(&code), unescape(&name));
    let r = lint_fixture(&name).await?;
    let codes: Vec<&str> = r.issues.iter().map(|i| i.code.as_str()).collect();
    w.ensure(codes.contains(&code.as_str()), || {
        format!(
            "expected rift-lint to report {code} for {name}, got {codes:?}: {}",
            r.raw
        )
    })
}

#[then(regex = r#"^rift-lint reports no "((?:\\.|[^"\\])*)" for fixture "((?:\\.|[^"\\])*)"$"#)]
async fn lint_reports_no(w: &mut PlaneA, code: String, name: String) -> Step {
    let (code, name) = (unescape(&code), unescape(&name));
    let r = lint_fixture(&name).await?;
    let codes: Vec<&str> = r.issues.iter().map(|i| i.code.as_str()).collect();
    w.ensure(!codes.contains(&code.as_str()) && r.exit_code == 0, || {
        format!(
            "expected rift-lint to report no {code} (and exit 0) for {name}, got {codes:?} exit {}: {}",
            r.exit_code, r.raw
        )
    })
}

// ── spaces and FSM scenarios (feature 07) ────────────────────────────────────────────────────

/// An admin request on the current imposter (`{admin}/imposters/{port}{suffix}`), kept as the
/// last response only by the steps that want it.
async fn imposter_admin(
    w: &mut PlaneA,
    method: &str,
    suffix: &str,
    body: Option<Value>,
) -> anyhow::Result<Response> {
    let url = format!("{}/imposters/{}{suffix}", w.admin_url(), w.current()?);
    let (headers, body) = match body {
        Some(b) => (json_content_type(), Some(b.to_string())),
        None => (Vec::new(), None),
    };
    w.record_curl(method, &url, &headers, body.as_deref());
    let (resp, _) = exchange(
        &clients().verifying,
        method,
        &url,
        &headers,
        body.as_deref(),
    )
    .await?
    .with_context(|| format!("transport failure on {method} {url}"))?;
    Ok(resp)
}

/// `POST .../spaces/{space}/stubs` with a minimal `equals path` / `is 200 body` stub.
#[when(
    regex = r#"^the client registers a stub in space "((?:\\.|[^"\\])*)" for path "((?:\\.|[^"\\])*)" returning "((?:\\.|[^"\\])*)"$"#
)]
async fn register_space_stub(w: &mut PlaneA, space: String, path: String, body: String) -> Step {
    let space = unescape(&space);
    let stub = serde_json::json!({
        "predicates": [{"equals": {"path": unescape(&path)}}],
        "responses": [{"is": {"statusCode": 200, "body": unescape(&body)}}]
    });
    let resp = imposter_admin(w, "POST", &format!("/spaces/{space}/stubs"), Some(stub)).await?;
    w.ensure(resp.status == 201, || {
        format!(
            "POST spaces/{space}/stubs -> {} (expected 201): {}",
            resp.status, resp.body
        )
    })
}

#[then(regex = r#"^space "((?:\\.|[^"\\])*)" has (\d+) stubs$"#)]
async fn space_stub_count(w: &mut PlaneA, space: String, n: usize) -> Step {
    let space = unescape(&space);
    let resp = imposter_admin(w, "GET", &format!("/spaces/{space}/stubs"), None).await?;
    let json: Value = serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
    let got = json
        .get("stubs")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| anyhow!("no 'stubs' array in spaces/{space} response: {}", resp.body))?;
    w.ensure(got == n, || {
        format!("expected space '{space}' to have {n} stub(s), got {got}")
    })
}

#[when(regex = r#"^the client tears down space "((?:\\.|[^"\\])*)"$"#)]
async fn tear_down_space(w: &mut PlaneA, space: String) -> Step {
    let space = unescape(&space);
    let resp = imposter_admin(w, "DELETE", &format!("/spaces/{space}"), None).await?;
    w.ensure(resp.status == 200, || {
        format!(
            "DELETE spaces/{space} -> {} (expected 200): {}",
            resp.status, resp.body
        )
    })
}

/// The named scenario's state from `GET .../scenarios[?flowId=]`, or `None` when it is not listed
/// (a flow that was never touched).
async fn scenario_state(
    w: &mut PlaneA,
    name: &str,
    flow_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let query = flow_id.map_or_else(String::new, |f| format!("?flowId={f}"));
    let resp = imposter_admin(w, "GET", &format!("/scenarios{query}"), None).await?;
    let json: Value = serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
    let scenarios = json
        .get("scenarios")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no 'scenarios' array in response: {}", resp.body))?;
    Ok(scenarios
        .iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

#[then(regex = r#"^scenario "((?:\\.|[^"\\])*)" is in state "((?:\\.|[^"\\])*)"$"#)]
async fn scenario_in_state(w: &mut PlaneA, name: String, state: String) -> Step {
    let (name, state) = (unescape(&name), unescape(&state));
    let got = scenario_state(w, &name, None).await?;
    w.ensure(got.as_deref() == Some(state.as_str()), || {
        format!("expected scenario '{name}' state '{state}', got {got:?}")
    })
}

#[then(
    regex = r#"^scenario "((?:\\.|[^"\\])*)" in space "((?:\\.|[^"\\])*)" is in state "((?:\\.|[^"\\])*)"$"#
)]
async fn scenario_in_space_in_state(
    w: &mut PlaneA,
    name: String,
    space: String,
    state: String,
) -> Step {
    let (name, space, state) = (unescape(&name), unescape(&space), unescape(&state));
    let got = scenario_state(w, &name, Some(&space)).await?;
    w.ensure(got.as_deref() == Some(state.as_str()), || {
        format!("expected scenario '{name}' in space '{space}' state '{state}', got {got:?}")
    })
}

#[then(regex = r#"^scenario "((?:\\.|[^"\\])*)" in space "((?:\\.|[^"\\])*)" is untouched$"#)]
async fn scenario_in_space_untouched(w: &mut PlaneA, name: String, space: String) -> Step {
    let (name, space) = (unescape(&name), unescape(&space));
    let got = scenario_state(w, &name, Some(&space)).await?;
    w.ensure(got.is_none() || got.as_deref() == Some("Started"), || {
        format!(
            "expected scenario '{name}' in space '{space}' to be untouched (absent or 'Started'), got {got:?}"
        )
    })
}

#[when(regex = r"^the client resets scenarios$")]
async fn reset_scenarios(w: &mut PlaneA) -> Step {
    let resp = imposter_admin(w, "POST", "/scenarios/reset", Some(serde_json::json!({}))).await?;
    w.ensure(resp.status == 200, || {
        format!(
            "POST scenarios/reset -> {} (expected 200): {}",
            resp.status, resp.body
        )
    })
}

#[when(regex = r#"^the client sets scenario "((?:\\.|[^"\\])*)" state to "((?:\\.|[^"\\])*)"$"#)]
async fn set_scenario_state(w: &mut PlaneA, name: String, state: String) -> Step {
    let name = unescape(&name);
    let body = serde_json::json!({"state": unescape(&state)});
    let resp = imposter_admin(w, "PUT", &format!("/scenarios/{name}/state"), Some(body)).await?;
    w.ensure(resp.status == 200, || {
        format!(
            "PUT scenarios/{name}/state -> {} (expected 200): {}",
            resp.status, resp.body
        )
    })
}

/// Fire `n` GETs at the current imposter at once, left running: the burst step joins them.
#[when(regex = r#"^the client fires (\d+) concurrent requests to "((?:\\.|[^"\\])*)"$"#)]
async fn fire_concurrent(w: &mut PlaneA, n: usize, path: String) -> Step {
    let port = w.current()?;
    let url = format!(
        "http://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    let headers = w.pending.headers.clone();
    let body = w.pending.body.clone();
    w.record_curl("GET", &url, &headers, body.as_deref());
    w.curl.push(format!("(x{n} concurrent, in flight)"));
    let requests = (0..n)
        .map(|_| {
            let (url, headers, body) = (url.clone(), headers.clone(), body.clone());
            tokio::spawn(async move {
                match exchange(&clients().verifying, "GET", &url, &headers, body.as_deref()).await {
                    Ok(Ok((resp, _))) => Ok(resp.status),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(e) => Err(format!("{e:#}")),
                }
            })
        })
        .collect();
    w.burst = Some(Burst {
        requested: n,
        requests,
    });
    Ok(())
}

#[then(regex = r"^all concurrent responses have status (\d+)$")]
async fn concurrent_statuses(w: &mut PlaneA, code: u16) -> Step {
    let burst = w
        .burst
        .take()
        .ok_or_else(|| anyhow!("no concurrent burst fired in this scenario"))?;
    let mut outcomes = Vec::with_capacity(burst.requests.len());
    for request in burst.requests {
        outcomes.push(
            request
                .await
                .map_err(|e| format!("request task: {e}"))
                .and_then(|r| r),
        );
    }
    let statuses: Vec<u16> = outcomes
        .iter()
        .filter_map(|o| o.as_ref().ok().copied())
        .collect();
    w.ensure(
        statuses.len() == burst.requested && statuses.iter().all(|s| *s == code),
        || {
            format!(
                "expected all {} responses to be {code}, got {outcomes:?}",
                burst.requested
            )
        },
    )
}

// ── reload (feature 11, rift#316) ────────────────────────────────────────────────────────────

/// A controlled 2-imposter config: one records requests (runtime state an incremental reload must
/// keep), one is static. The Scala runner's `reloadConfigJson`, byte for byte.
const RELOAD_CONFIG: &str = r#"{"imposters":[{"port":4870,"protocol":"http","recordRequests":true,"stubs":[{"predicates":[{"equals":{"path":"/a"}}],"responses":[{"is":{"statusCode":200,"body":"A"}}]}]},{"port":4871,"protocol":"http","stubs":[{"predicates":[{"equals":{"path":"/b"}}],"responses":[{"is":{"statusCode":200,"body":"B"}}]}]}]}"#;

/// The `@front-door` config (rift#1160): imposters answering `A` (4872) and `B` (4873) plus a
/// `routes` block holding `routes`.
fn front_door_config(routes: &[Value]) -> String {
    let imposter = |port: u16, body: &str| {
        serde_json::json!({"port": port, "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": body}}]}]})
    };
    serde_json::json!({
        "imposters": [imposter(4872, "A"), imposter(4873, "B")],
        "routes": {"routes": routes}
    })
    .to_string()
}

fn host_route(id: &str, host: &str, port: u16) -> Value {
    serde_json::json!({"id": id, "match": {"host": host}, "target": {"port": port}})
}

async fn start_reload_engine(w: &mut PlaneA, json: &str, front_door: bool) -> Step {
    if let Some(old) = w.reload.take() {
        old.stop().await?;
    }
    w.reload = Some(harness().start_reload_engine(json, front_door).await?);
    Ok(())
}

fn reload_engine(w: &PlaneA) -> anyhow::Result<&ReloadEngine> {
    w.reload
        .as_ref()
        .ok_or_else(|| anyhow!("no reload engine started in this scenario"))
}

#[given(regex = r"^the engine is started with the reload config file$")]
async fn start_with_reload_config(w: &mut PlaneA) -> Step {
    start_reload_engine(w, RELOAD_CONFIG, false).await
}

#[given(regex = r#"^the engine is started with the reload fixture "((?:\\.|[^"\\])*)"$"#)]
async fn start_with_reload_fixture(w: &mut PlaneA, name: String) -> Step {
    let json = load_fixture(&unescape(&name))?;
    start_reload_engine(w, &json, false).await
}

#[given(
    regex = r#"^the engine is started with the front door and a route from host "((?:\\.|[^"\\])*)" to port (\d+)$"#
)]
async fn start_with_front_door(w: &mut PlaneA, host: String, port: u16) -> Step {
    let json = front_door_config(&[host_route("r", &unescape(&host), port)]);
    start_reload_engine(w, &json, true).await
}

#[given(
    regex = r#"^the engine is started without the front door and a route from host "((?:\\.|[^"\\])*)" to port (\d+)$"#
)]
async fn start_without_front_door(w: &mut PlaneA, host: String, port: u16) -> Step {
    let json = front_door_config(&[host_route("r", &unescape(&host), port)]);
    start_reload_engine(w, &json, false).await
}

#[when(regex = r#"^the reload config file routes host "((?:\\.|[^"\\])*)" to port (\d+)$"#)]
async fn reload_config_routes(w: &mut PlaneA, host: String, port: u16) -> Step {
    let json = front_door_config(&[host_route("r", &unescape(&host), port)]);
    Ok(reload_engine(w)?.rewrite_config(&json)?)
}

#[when(regex = r"^the reload config file declares two routes with the same id$")]
async fn reload_config_duplicate_routes(w: &mut PlaneA) -> Step {
    let json = front_door_config(&[
        host_route("dup", "c.test", 4872),
        host_route("dup", "d.test", 4873),
    ]);
    Ok(reload_engine(w)?.rewrite_config(&json)?)
}

#[when(regex = r#"^the reload config file is rewritten to fixture "((?:\\.|[^"\\])*)"$"#)]
async fn reload_config_to_fixture(w: &mut PlaneA, name: String) -> Step {
    let json = load_fixture(&unescape(&name))?;
    Ok(reload_engine(w)?.rewrite_config(&json)?)
}

#[when(regex = r"^the reload config file is rewritten to invalid JSON$")]
async fn reload_config_invalid(w: &mut PlaneA) -> Step {
    Ok(reload_engine(w)?.rewrite_config(r#"{"imposters":[ not json"#)?)
}

/// `POST {admin}/admin/reload`, its response kept whatever the outcome.
async fn attempt_reload(w: &mut PlaneA) -> Step {
    let url = format!("{}/admin/reload", w.admin_url());
    w.send_with(&clients().verifying, "POST", &url, &[], None)
        .await
}

#[when(regex = r"^the engine reloads its configuration$")]
async fn reload(w: &mut PlaneA) -> Step {
    attempt_reload(w).await?;
    let r = w.last()?;
    w.ensure(r.status == 200, || {
        format!(
            "POST admin/reload -> {} (expected 200): {}",
            r.status, r.body
        )
    })
}

#[when(regex = r"^the engine reload is attempted$")]
async fn reload_attempted(w: &mut PlaneA) -> Step {
    attempt_reload(w).await
}

#[then(regex = r#"^the reload is refused naming "((?:\\.|[^"\\])*)"$"#)]
async fn reload_refused(w: &mut PlaneA, reason: String) -> Step {
    let reason = unescape(&reason);
    let r = w.last()?;
    w.ensure(r.status >= 400 && r.body.contains(&reason), || {
        format!(
            "expected the reload refused (status >= 400) naming '{reason}', got {}: {}",
            r.status, r.body
        )
    })
}

#[then(regex = r#"^the reload report warns about "((?:\\.|[^"\\])*)"$"#)]
async fn reload_warns(w: &mut PlaneA, text: String) -> Step {
    let text = unescape(&text);
    let json = w.last_json()?;
    let warnings: Option<Vec<&str>> = json
        .get("warnings")
        .and_then(Value::as_array)
        .map(|ws| ws.iter().filter_map(Value::as_str).collect());
    w.ensure(
        warnings
            .as_ref()
            .is_some_and(|ws| ws.iter().any(|x| x.contains(&text))),
        || format!("expected a reload warning naming '{text}', got {warnings:?}"),
    )
}

/// The field is unquoted in the feature (`created`, `deleted`, ...), as zio-bdd's `string` allows.
#[then(regex = r"^the reload report (\w+) (\d+) imposters$")]
async fn reload_report_count(w: &mut PlaneA, field: String, n: usize) -> Step {
    let json = w.last_json()?;
    let got = json
        .get(&field)
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| anyhow!("no '{field}' array in reload report: {json}"))?;
    w.ensure(got == n, || {
        format!("expected reload report '{field}' to have length {n}, got {got}")
    })
}

#[then(regex = r"^the reload report (\w+) port (\d+)$")]
async fn reload_report_port(w: &mut PlaneA, field: String, port: i64) -> Step {
    let json = w.last_json()?;
    let ports: Option<Vec<i64>> = json
        .get(&field)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect());
    w.ensure(ports.as_deref() == Some(&[port][..]), || {
        format!("expected reload report '{field}' == [{port}], got {ports:?}")
    })
}

#[then(regex = r"^the reload report exposes all four change fields$")]
async fn reload_report_fields(w: &mut PlaneA) -> Step {
    let json = w.last_json()?;
    let keys: Vec<&str> = json
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let required = ["created", "replaced", "stubPatched", "deleted"];
    w.ensure(required.iter().all(|k| keys.contains(k)), || {
        format!("expected reload report to expose {required:?}, got keys={keys:?} (body={json})")
    })
}

#[then(regex = r#"^the reload report message is "((?:\\.|[^"\\])*)"$"#)]
async fn reload_report_message(w: &mut PlaneA, message: String) -> Step {
    let message = unescape(&message);
    let Value::Object(map) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON object: {}", w.last()?.body).into());
    };
    let got = map.get("message").and_then(Value::as_str);
    w.ensure(got == Some(message.as_str()), || {
        format!("expected reload message '{message}', got {got:?}")
    })
}

/// Over plain HTTP with an explicit `Host`, which is what the front door routes on.
#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the front door with host "((?:\\.|[^"\\])*)"$"#
)]
async fn send_through_front_door(
    w: &mut PlaneA,
    method: String,
    path: String,
    host: String,
) -> Step {
    front_door_send(w, &method, &unescape(&path), &unescape(&host), Vec::new()).await
}

/// The scenario's front door port.
fn front_door_port(w: &PlaneA) -> anyhow::Result<u16> {
    reload_engine(w)?
        .front_door_port
        .ok_or_else(|| anyhow!("no front-door engine started in this scenario"))
}

/// `headers` go out in order after `Host`, a repeated name as separate lines: the headers clause
/// compares the first value only.
async fn front_door_send(
    w: &mut PlaneA,
    method: &str,
    path: &str,
    host: &str,
    headers: Vec<(String, String)>,
) -> Step {
    let port = front_door_port(w)?;
    let url = format!("http://{}:{port}{path}", harness().imposter_host());
    let mut all = vec![("Host".to_string(), host.to_string())];
    all.extend(headers);
    w.send_with(&clients().verifying, method, &url, &all, None)
        .await
}

#[then(regex = r"^the imposter on port (\d+) no longer serves$")]
async fn port_no_longer_serves(w: &mut PlaneA, port: u16) -> Step {
    let connect = tokio::net::TcpStream::connect((harness().imposter_host(), port));
    // Only a refused connection proves the listener is gone; a timeout or any other error proves
    // nothing either way.
    match tokio::time::timeout(Duration::from_secs(1), connect).await {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(()),
        Ok(Ok(_)) => w.ensure(false, || {
            format!("expected port {port} to be gone after the reload, but it still accepts")
        }),
        Ok(Err(e)) => Err(anyhow!("connecting to port {port}: {e}").into()),
        Err(_) => Err(anyhow!("connecting to port {port} timed out after 1s").into()),
    }
}

// ── metrics (feature 17, #192) ───────────────────────────────────────────────────────────────

async fn scrape(listener: &str) -> anyhow::Result<PromText> {
    let h = harness();
    let base = match listener {
        "metrics" => &h.metrics_url,
        "admin" => &h.admin_url,
        other => bail!("unknown listener '{other}' (metrics|admin)"),
    };
    let url = format!("{base}/metrics");
    let resp = get(&url).await?;
    if resp.status != 200 {
        let head: String = resp.body.chars().take(200).collect();
        bail!("GET {url} -> {}: {head}", resp.status);
    }
    PromText::parse(&resp.body).map_err(|e| anyhow!("{url}: {e}"))
}

/// A selector with `{port}` filled from the scenario's current imposter.
fn series_of(w: &PlaneA, sel: &str) -> anyhow::Result<Series> {
    let filled = match w.current {
        Some(p) => sel.replace("{port}", &p.to_string()),
        None => sel.to_string(),
    };
    prom::selector(&filled).map_err(|e| anyhow!(e))
}

/// How far `sel` moved since the snapshot on `listener`, re-scraped 100 ms apart (up to 20 more
/// times) until `done` holds: the engine may record a beat after the response. A series absent
/// before counts as 0, since a labelled counter is born on its first increment.
async fn delta(
    w: &PlaneA,
    listener: &str,
    sel: &str,
    done: impl Fn(f64) -> bool,
) -> anyhow::Result<(Series, f64)> {
    let series = series_of(w, sel)?;
    let before = w
        .snapshots
        .get(listener)
        .ok_or_else(|| anyhow!("no snapshot of the '{listener}' listener taken"))?
        .get(&series)
        .unwrap_or(0.0);
    let mut retries = 0;
    loop {
        let d = scrape(listener).await?.get(&series).unwrap_or(0.0) - before;
        if done(d) || retries == 20 {
            return Ok((series, d));
        }
        retries += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[when(regex = r#"^the client snapshots the "((?:\\.|[^"\\])*)" listener$"#)]
async fn snapshot_listener(w: &mut PlaneA, listener: String) -> Step {
    let listener = unescape(&listener);
    let parsed = scrape(&listener).await?;
    w.snapshots.insert(listener, parsed);
    Ok(())
}

#[then(regex = r#"^on the "((?:\\.|[^"\\])*)" listener "((?:\\.|[^"\\])*)" grew by (\d+)$"#)]
async fn grew_by(w: &mut PlaneA, listener: String, sel: String, n: u32) -> Step {
    let n = f64::from(n);
    let (s, d) = delta(w, &unescape(&listener), &unescape(&sel), |d| d >= n).await?;
    w.ensure(d == n, || {
        format!("expected {s} to grow by exactly {n}, it grew by {d}")
    })
}

#[then(
    regex = r#"^on the "((?:\\.|[^"\\])*)" listener "((?:\\.|[^"\\])*)" grew by at least (\d+)$"#
)]
async fn grew_by_at_least(w: &mut PlaneA, listener: String, sel: String, n: u32) -> Step {
    let n = f64::from(n);
    let (s, d) = delta(w, &unescape(&listener), &unescape(&sel), |d| d >= n).await?;
    w.ensure(d >= n, || {
        format!("expected {s} to grow by at least {n}, it grew by {d}")
    })
}

#[then(regex = r#"^the metrics listener exports "((?:\\.|[^"\\])*)"$"#)]
async fn metrics_exports(w: &mut PlaneA, sel: String) -> Step {
    let s = series_of(w, &unescape(&sel))?;
    let p = scrape("metrics").await?;
    w.ensure(p.get(&s).is_some(), || {
        format!("expected {s} to be exported; it is absent")
    })
}

#[then(regex = r#"^the metrics listener reports "((?:\\.|[^"\\])*)" as (\d+)$"#)]
async fn metrics_reports(w: &mut PlaneA, sel: String, n: u32) -> Step {
    let s = series_of(w, &unescape(&sel))?;
    let v = scrape("metrics").await?.get(&s);
    w.ensure(v == Some(f64::from(n)), || {
        format!(
            "expected {s} = {n}, got {}",
            v.map_or_else(|| "<absent>".to_string(), |v| v.to_string())
        )
    })
}

#[then(
    regex = r#"^admin metric "((?:\\.|[^"\\])*)" equals the number of imposters listed by GET /imposters$"#
)]
async fn admin_metric_equals_listed(w: &mut PlaneA, sel: String) -> Step {
    let s = series_of(w, &unescape(&sel))?;
    let v = scrape("admin").await?.get(&s);
    let url = format!("{}/imposters", harness().admin_url);
    let body = get(&url).await?.body;
    let json: Value =
        serde_json::from_str(&body).with_context(|| format!("invalid admin JSON (body={body})"))?;
    let n = json_at(&json, "imposters")
        .and_then(Value::as_array)
        .map(Vec::len);
    w.ensure(n.is_some_and(|n| v == Some(n as f64)), || {
        format!(
            "expected {s} to equal the GET /imposters count {}, got {}",
            n.map_or_else(|| "<no imposters array>".to_string(), |n| n.to_string()),
            v.map_or_else(|| "<absent>".to_string(), |v| v.to_string())
        )
    })
}

// #1045: connect, then hang up before sending a byte.
#[when(regex = r"^a raw TCP connection to the current imposter closes without sending$")]
async fn raw_connect_and_close(w: &mut PlaneA) -> Step {
    let port = w.current()?;
    let host = harness().imposter_host();
    let connect = tokio::net::TcpStream::connect((host, port));
    match tokio::time::timeout(Duration::from_secs(2), connect).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(())
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            w.ensure(false, || format!("connect to {host}:{port} was refused"))
        }
        Ok(Err(e)) => Err(anyhow::Error::new(e)
            .context(format!("connecting to {host}:{port}"))
            .into()),
        Err(_) => Err(anyhow!("connecting to {host}:{port} timed out after 2s").into()),
    }
}

// ── error envelope (rift#611; features 03, 10, 19, 21) ───────────────────────────────────────

/// One entry of the error envelope `{"errors":[{"code","type","message"}]}`. `slug` is 0.15.0's
/// `type` (rift#797): absent on an older engine, which fails the step that names it.
struct EnvelopeEntry {
    code: String,
    slug: Option<String>,
    message: String,
}

/// The envelope out of `body`, or why the body is not one (unparseable vs the wrong shape), so a
/// failure reads as the actual defect.
fn parse_error_envelope(body: &str) -> Result<Vec<EnvelopeEntry>, String> {
    let root: Value = serde_json::from_str(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let Value::Object(map) = root else {
        return Err("body root is not a JSON object".to_string());
    };
    let Some(Value::Array(entries)) = map.get("errors") else {
        return Err("no top-level 'errors' array".to_string());
    };
    if entries.is_empty() {
        return Err("'errors' array is empty".to_string());
    }
    entries
        .iter()
        .map(|entry| {
            let Value::Object(f) = entry else {
                return Err(format!("'errors' entry is not an object: {entry}"));
            };
            match (
                f.get("code").and_then(Value::as_str),
                f.get("message").and_then(Value::as_str),
            ) {
                (Some(c), Some(m)) => Ok(EnvelopeEntry {
                    code: c.to_string(),
                    slug: f.get("type").and_then(Value::as_str).map(str::to_string),
                    message: m.to_string(),
                }),
                _ => Err(format!("entry lacks a string 'code'/'message': {entry}")),
            }
        })
        .collect()
}

impl PlaneA {
    /// The last response's envelope, failing with the parse diagnosis when the body is not one.
    fn envelope(&self) -> anyhow::Result<Vec<EnvelopeEntry>> {
        let body = &self.last()?.body;
        parse_error_envelope(body).map_err(|why| {
            anyhow!(
                "not a rift error envelope ({why}): '{body}'\n--- repro ---\n{}",
                self.curl.join("\n")
            )
        })
    }
}

// Both halves: the pre-sweep defect was an undecodable body, which a substring assert on the same
// body passes either way (feature 13, #118).
#[then(regex = r"^the response is a rift error envelope$")]
async fn response_is_envelope(w: &mut PlaneA) -> Step {
    let ct = w.last()?.header_values("content-type").join(",");
    w.ensure(ct.to_lowercase().starts_with("application/json"), || {
        format!(
            "expected Content-Type: application/json, got {}",
            if ct.is_empty() { "<absent>" } else { &ct }
        )
    })?;
    let entries = w.envelope()?;
    let body = w.last()?.body.clone();
    w.ensure(entries.iter().all(|e| !e.message.is_empty()), || {
        format!("envelope carries an empty message: '{body}'")
    })
}

#[then(regex = r#"^the error envelope type is "((?:\\.|[^"\\])*)"$"#)]
async fn envelope_type_is(w: &mut PlaneA, slug: String) -> Step {
    let slug = unescape(&slug);
    let entries = w.envelope()?;
    w.ensure(
        entries
            .iter()
            .any(|e| e.slug.as_deref() == Some(slug.as_str())),
        || {
            let got: Vec<&str> = entries
                .iter()
                .map(|e| e.slug.as_deref().unwrap_or("<absent>"))
                .collect();
            format!("expected envelope type '{slug}', got {}", got.join(", "))
        },
    )
}

// Reads the DECODED message, so it passes only if a message holding a `"` round-tripped through a
// well-formed envelope (the rift#611 regression).
#[then(regex = r#"^the error envelope message contains "((?:\\.|[^"\\])*)"$"#)]
async fn envelope_message_contains(w: &mut PlaneA, sub: String) -> Step {
    let sub = unescape(&sub);
    let entries = w.envelope()?;
    w.ensure(entries.iter().any(|e| e.message.contains(&sub)), || {
        let got: Vec<&str> = entries.iter().map(|e| e.message.as_str()).collect();
        format!(
            "expected an envelope message containing '{sub}', got {}",
            got.join(" | ")
        )
    })
}

// ── JSON reads (features 03, 10, 19, 21) ─────────────────────────────────────────────────────

/// A step's JSON literal (`"\"f1\""`, `true`, `{"a":1}`), which must parse.
fn json_literal(literal: &str) -> anyhow::Result<Value> {
    serde_json::from_str(literal).with_context(|| format!("step literal '{literal}' is not JSON"))
}

// Compared as parsed JSON: key order aside (serde_json's maps compare as maps), and `1.0` is not
// `1`, as the Scala runner's `Json.Num` compared them.
#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#)]
async fn json_at_is(w: &mut PlaneA, path: String, literal: String) -> Step {
    let (path, literal) = (unescape(&path), unescape(&literal));
    let want = json_literal(&literal)?;
    let root = w.last_json()?;
    let got = json_at(&root, &path);
    w.ensure(got == Some(&want), || {
        format!(
            "at '{path}': expected {literal}, got {}",
            got.map_or_else(|| "<absent>".to_string(), Value::to_string)
        )
    })
}

#[then(regex = r#"^the response JSON field "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#)]
async fn json_field_is(w: &mut PlaneA, field: String, literal: String) -> Step {
    let (field, literal) = (unescape(&field), unescape(&literal));
    let want = json_literal(&literal)?;
    let Value::Object(map) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON object: {}", w.last()?.body).into());
    };
    let got = map.get(&field);
    w.ensure(got == Some(&want), || {
        format!(
            "expected '{field}' = {literal}, got {}",
            got.map_or_else(|| "<absent>".to_string(), Value::to_string)
        )
    })
}

#[then(
    regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" equals the same path in fixture "((?:\\.|[^"\\])*)"$"#
)]
async fn json_at_equals_fixture(w: &mut PlaneA, path: String, name: String) -> Step {
    let (path, name) = (unescape(&path), unescape(&name));
    let fixture: Value = serde_json::from_str(&load_fixture(&name)?)
        .with_context(|| format!("fixture {name} is not JSON"))?;
    let want = json_at(&fixture, &path)
        .ok_or_else(|| anyhow!("fixture {name} has nothing at '{path}'"))?;
    let root = w.last_json()?;
    let got = json_at(&root, &path);
    w.ensure(got == Some(want), || {
        format!(
            "at '{path}': expected {want}, got {}",
            got.map_or_else(|| "<absent>".to_string(), Value::to_string)
        )
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is a non-empty string$"#)]
async fn json_at_non_empty_string(w: &mut PlaneA, path: String) -> Step {
    w.ensure_json_at(&unescape(&path), "a non-empty string", |v| {
        v.as_str().is_some_and(|s| !s.is_empty())
    })
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is present$"#)]
async fn json_at_present(w: &mut PlaneA, path: String) -> Step {
    w.ensure_json_at(&unescape(&path), "a value", |_| true)
}

#[then(regex = r#"^the response JSON at "((?:\\.|[^"\\])*)" is a number at least (\d+)$"#)]
async fn json_at_number_at_least(w: &mut PlaneA, path: String, floor: u32) -> Step {
    w.ensure_json_at(&unescape(&path), &format!("a number >= {floor}"), |v| {
        v.as_f64().is_some_and(|x| x >= f64::from(floor))
    })
}

#[then(regex = r#"^the response JSON has top-level field "((?:\\.|[^"\\])*)"$"#)]
async fn json_has_top_level_field(w: &mut PlaneA, field: String) -> Step {
    let field = unescape(&field);
    let Value::Object(map) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON object: {}", w.last()?.body).into());
    };
    w.ensure(map.contains_key(&field), || {
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        format!(
            "expected top-level field '{field}', got {}",
            keys.join(", ")
        )
    })
}

#[then(regex = r#"^the response body does not contain "((?:\\.|[^"\\])*)"$"#)]
async fn response_body_lacks(w: &mut PlaneA, sub: String) -> Step {
    let sub = unescape(&sub);
    let got = &w.last()?.body;
    w.ensure(!got.contains(&sub), || {
        format!("expected body without '{sub}', got '{got}'")
    })
}

#[then(regex = r#"^the response header "((?:\\.|[^"\\])*)" is a number at least (\d+)$"#)]
async fn response_header_number_at_least(w: &mut PlaneA, name: String, floor: i64) -> Step {
    let name = unescape(&name);
    let got = w.last()?.header_values(&name).first().cloned();
    w.ensure(
        got.as_deref()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .is_some_and(|n| n >= floor),
        || {
            format!(
                "expected header {name} to be a number >= {floor}, got {}",
                got.as_deref().unwrap_or("<absent>")
            )
        },
    )
}

#[then(regex = r"^the response took at most (\d+) ms$")]
async fn response_took_at_most(w: &mut PlaneA, ceiling: u64) -> Step {
    let took = w
        .elapsed
        .ok_or_else(|| anyhow!("no timing: send a request first"))?;
    let ms = took.as_millis();
    w.ensure(ms <= u128::from(ceiling), || {
        format!("expected the request to take <= {ceiling} ms, took {ms} ms")
    })
}

// ── flow state (feature 19, #194; feature 05) ────────────────────────────────────────────────

impl PlaneA {
    /// `{admin}/admin/imposters/{port}/flow-state/{flow}/{key}` on the engine owning the current
    /// imposter: a dedicated engine's while one runs. `{port}` in the flow id is the current port.
    fn flow_state_url(&self, flow: &str, key: &str) -> anyhow::Result<String> {
        let port = self.current()?;
        let admin = self
            .side_engine
            .as_ref()
            .map_or_else(|| harness().admin_url.clone(), |e| e.admin_url.clone());
        let flow = flow.replace("{port}", &port.to_string());
        Ok(format!(
            "{admin}/admin/imposters/{port}/flow-state/{flow}/{key}"
        ))
    }

    /// GET a flow-state key: 200 with `{"value": ...}`, or 404.
    async fn flow_state(&mut self, flow: &str, key: &str) -> anyhow::Result<Response> {
        let url = self.flow_state_url(flow, key)?;
        self.record_curl("GET", &url, &[], None);
        get(&url).await
    }

    /// The value of a flow-state key that must exist.
    async fn flow_state_value(&mut self, flow: &str, key: &str) -> anyhow::Result<Option<Value>> {
        let resp = self.flow_state(flow, key).await?;
        self.ensure(resp.status == 200, || {
            format!(
                "GET flow-state {flow}/{key} -> {} (expected 200): {}",
                resp.status, resp.body
            )
        })
        .map_err(|f| f.0)?;
        let json: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
        Ok(json.get("value").cloned())
    }
}

#[when(
    regex = r#"^the client sets flow-state "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)" to "((?:\\.|[^"\\])*)"$"#
)]
async fn set_flow_state(w: &mut PlaneA, key: String, flow: String, value: String) -> Step {
    let url = w.flow_state_url(&unescape(&flow), &unescape(&key))?;
    let body = serde_json::json!({ "value": unescape(&value) }).to_string();
    let headers = json_content_type();
    w.record_curl("PUT", &url, &headers, Some(&body));
    let (resp, _) = exchange(&clients().verifying, "PUT", &url, &headers, Some(&body))
        .await?
        .with_context(|| format!("transport failure on PUT {url}"))?;
    w.ensure(resp.status == 200, || {
        format!("PUT {url} -> {} (expected 200): {}", resp.status, resp.body)
    })
}

#[then(
    regex = r#"^flow-state "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)" is "((?:\\.|[^"\\])*)"$"#
)]
async fn flow_state_is_string(w: &mut PlaneA, key: String, flow: String, value: String) -> Step {
    let (key, flow, want) = (unescape(&key), unescape(&flow), unescape(&value));
    let got = w.flow_state_value(&flow, &key).await?;
    w.ensure(
        got.as_ref().and_then(Value::as_str) == Some(want.as_str()),
        || {
            format!(
                "expected flow-state {flow}/{key} = '{want}', got {}",
                got.map_or_else(|| "<no value>".to_string(), |v| v.to_string())
            )
        },
    )
}

// Typed: `set "42"` must store the JSON number 42, so a string "42" fails here.
#[then(
    regex = r#"^flow-state "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)" is the number (-?\d+)$"#
)]
async fn flow_state_is_number(w: &mut PlaneA, key: String, flow: String, n: i64) -> Step {
    let (key, flow) = (unescape(&key), unescape(&flow));
    let got = w.flow_state_value(&flow, &key).await?;
    w.ensure(got.as_ref().is_some_and(|v| is_number(v, n)), || {
        format!(
            "expected flow-state {flow}/{key} = the number {n}, got {}",
            got.map_or_else(|| "<no value>".to_string(), |v| v.to_string())
        )
    })
}

#[then(regex = r#"^flow-state "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)" is absent$"#)]
async fn flow_state_absent(w: &mut PlaneA, key: String, flow: String) -> Step {
    let (key, flow) = (unescape(&key), unescape(&flow));
    let resp = w.flow_state(&flow, &key).await?;
    w.ensure(resp.status == 404, || {
        format!(
            "expected 404 for flow-state {flow}/{key}, got {}: {}",
            resp.status, resp.body
        )
    })
}

/// Features 05 and 19 `@rift-debug-env`: a dedicated engine started with `flag`, holding the
/// fixture as the current imposter. Not tracked: the whole engine is killed after the scenario.
#[given(
    regex = r#"^a fresh imposter from fixture "((?:\\.|[^"\\])*)" on an engine started with "((?:\\.|[^"\\])*)"$"#
)]
async fn fresh_imposter_on_flag_engine(w: &mut PlaneA, name: String, flag: String) -> Step {
    let (name, flag) = (unescape(&name), unescape(&flag));
    if let Some(old) = w.side_engine.take() {
        old.stop().await?;
    }
    let engine = harness()
        .spawn_flag_engine(std::slice::from_ref(&flag))
        .await?;
    let admin = engine.admin_url.clone();
    w.side_engine = Some(engine);
    let raw = harness().rewrite_fixture(&load_fixture(&name)?);
    let port = post_assigned(&admin, &raw, &format!("fixture {name} on a {flag} engine")).await?;
    w.current = Some(port);
    Ok(())
}

/// POST an imposter without its `port` to `admin`, returning the port the engine assigned.
async fn post_assigned(admin: &str, raw: &str, what: &str) -> anyhow::Result<u16> {
    let mut map = parse_object(raw, what)?;
    map.remove("port");
    let (status, body) = post_imposter(admin, &Value::Object(map).to_string()).await?;
    if status != 201 {
        bail!("POST imposter ({what}) returned {status}, expected 201: {body}");
    }
    declared_port(&body, &format!("the 201 for {what}"))
}

// ── templating (feature 05) ──────────────────────────────────────────────────────────────────

/// An ISO date-time with an offset (its local date), or a bare ISO date.
fn iso_date(body: &str) -> Option<chrono::NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(body)
        .map(|d| d.date_naive())
        .ok()
        .or_else(|| {
            (body.len() == 10)
                .then(|| chrono::NaiveDate::parse_from_str(body, "%Y-%m-%d").ok())
                .flatten()
        })
}

/// The expected date is computed at assertion time, so the step never goes stale, and it fails
/// only when the engine's date arithmetic is wrong. `unit` may be quoted or a bare outline cell.
#[then(regex = r"^the response body is a date offset by (-?\d+) (.+) from today$")]
async fn body_is_date_offset(w: &mut PlaneA, offset: i32, unit: String) -> Step {
    let unit = flexible_string(&unit);
    let body = w.last()?.body.trim().to_string();
    let got = iso_date(&body).ok_or_else(|| anyhow!("body '{body}' is not an ISO date(-time)"))?;
    let today = chrono::Utc::now().date_naive();
    let magnitude = offset.unsigned_abs();
    let want = match unit.as_str() {
        "days" => {
            let days = chrono::Days::new(u64::from(magnitude));
            if offset < 0 {
                today.checked_sub_days(days)
            } else {
                today.checked_add_days(days)
            }
        }
        "months" => {
            let months = chrono::Months::new(magnitude);
            if offset < 0 {
                today.checked_sub_months(months)
            } else {
                today.checked_add_months(months)
            }
        }
        other => return Err(anyhow!("unknown date unit '{other}' (days|months)").into()),
    }
    .ok_or_else(|| anyhow!("{today} offset by {offset} {unit} is out of range"))?;
    w.ensure(got == want, || {
        format!("expected a date {offset} {unit} from today ({want}), got {got}")
    })
}

#[when(regex = r"^the client remembers the response body$")]
async fn remember_body(w: &mut PlaneA) -> Step {
    w.remembered = Some(w.last()?.body.clone());
    Ok(())
}

#[then(regex = r"^the response body differs from the remembered one$")]
async fn body_differs(w: &mut PlaneA) -> Step {
    let prev = w
        .remembered
        .clone()
        .ok_or_else(|| anyhow!("no remembered body"))?;
    let got = w.last()?.body.clone();
    w.ensure(got != prev, || {
        format!("expected a body other than '{prev}', got the same")
    })
}

// ── proxy record/replay (feature 10) ─────────────────────────────────────────────────────────

/// `GET {admin}/imposters/{port}` on the shared engine, as JSON.
async fn imposter_json(w: &PlaneA, port: u16) -> anyhow::Result<Value> {
    let url = format!("{}/imposters/{port}", harness().admin_url);
    let resp = get(&url).await?;
    if resp.status != 200 {
        bail!(
            "GET {url} -> {} (expected 200): {}\n--- repro ---\n{}",
            resp.status,
            resp.body,
            w.curl.join("\n")
        );
    }
    serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))
}

async fn stub_count(w: &PlaneA, port: u16) -> anyhow::Result<usize> {
    let json = imposter_json(w, port).await?;
    json.get("stubs")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| anyhow!("no 'stubs' array in imposters/{port}: {json}"))
}

/// Recording lands a beat after the response, so this polls (every 100 ms, ~1.4 s) until the count
/// is `n` and asserts on the last count seen.
#[then(regex = r"^the imposter on port (\d+) has (\d+) stubs$")]
async fn imposter_stub_count(w: &mut PlaneA, port: u16, n: usize) -> Step {
    let mut got = stub_count(w, port).await?;
    for _ in 0..14 {
        if got == n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        got = stub_count(w, port).await?;
    }
    w.ensure(got == n, || {
        format!("expected imposter on port {port} to have {n} stub(s), got {got}")
    })
}

/// The negative: a poll that stops at the first match cannot show that nothing is recorded a beat
/// later, so this waits out the whole settle window, then reads once.
#[then(regex = r"^the imposter on port (\d+) still has (\d+) stubs after recording settles$")]
async fn imposter_stub_count_settled(w: &mut PlaneA, port: u16, n: usize) -> Step {
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let got = stub_count(w, port).await?;
    w.ensure(got == n, || {
        format!("expected imposter on port {port} to STILL have {n} stub(s), got {got}")
    })
}

#[when(regex = r"^the client tears down the imposter on port (\d+)$")]
async fn tear_down_imposter(_w: &mut PlaneA, port: u16) -> Step {
    delete_imposter(&harness().admin_url, port).await?;
    Ok(())
}

/// The first stub's `predicates`, compact JSON (the whole first stub when it has none).
#[then(
    regex = r#"^the imposter on port (\d+) first recorded predicate includes "((?:\\.|[^"\\])*)"$"#
)]
async fn first_predicate_includes(w: &mut PlaneA, port: u16, sub: String) -> Step {
    let sub = unescape(&sub);
    let json = imposter_json(w, port).await?;
    let first = json
        .get("stubs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no 'stubs' array in imposters/{port}: {json}"))?
        .first()
        .ok_or_else(|| anyhow!("imposters/{port} has no stubs"))?;
    let predicates = first
        .get("predicates")
        .filter(|p| p.is_array())
        .unwrap_or(first)
        .to_string();
    w.ensure(predicates.contains(&sub), || {
        format!(
            "expected imposter {port}'s first stub predicates to include '{sub}', got: {predicates}"
        )
    })
}

/// The upstream's journal (posted with `recordRequests: true`): the request the proxy forwarded.
async fn upstream_last_request(w: &PlaneA) -> anyhow::Result<serde_json::Map<String, Value>> {
    let up = upstream(w)?;
    let json = imposter_json(w, up).await?;
    match json
        .get("requests")
        .and_then(Value::as_array)
        .and_then(|r| r.last())
    {
        Some(Value::Object(fields)) => Ok(fields.clone()),
        Some(other) => bail!("recorded request is not an object: {other}"),
        None => bail!("upstream {up} recorded no request: {json}"),
    }
}

#[then(regex = r#"^the upstream imposter's last recorded request has path "((?:\\.|[^"\\])*)"$"#)]
async fn upstream_last_path(w: &mut PlaneA, path: String) -> Step {
    let want = unescape(&path);
    let fields = upstream_last_request(w).await?;
    let got = fields.get("path").and_then(Value::as_str);
    w.ensure(got == Some(want.as_str()), || {
        format!("expected the forwarded path '{want}', got {got:?}")
    })
}

#[then(
    regex = r#"^the upstream imposter's last recorded request has header "((?:\\.|[^"\\])*)" with value "((?:\\.|[^"\\])*)"$"#
)]
async fn upstream_last_header(w: &mut PlaneA, name: String, value: String) -> Step {
    let (name, want) = (unescape(&name), unescape(&value));
    let fields = upstream_last_request(w).await?;
    let headers = fields.get("headers").and_then(Value::as_object);
    let got: Vec<&str> = headers
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
        .flat_map(|(_, v)| match v {
            Value::String(s) => vec![s.as_str()],
            Value::Array(xs) => xs.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        })
        .collect();
    w.ensure(got == [want.as_str()], || {
        format!(
            "expected forwarded header {name} = [{want}], got {got:?} (headers={})",
            headers.map_or_else(
                || "<none>".to_string(),
                |h| Value::Object(h.clone()).to_string()
            )
        )
    })
}

// ── admin REST long tail (feature 21, #200) ──────────────────────────────────────────────────

impl PlaneA {
    fn named_port(&self, name: &str) -> anyhow::Result<u16> {
        self.named.get(name).copied().ok_or_else(|| {
            let have: Vec<&str> = self.named.keys().map(String::as_str).collect();
            anyhow!("no imposter named '{name}' (have {})", have.join(", "))
        })
    }

    /// The `imposters` array of the last response.
    fn listed(&self) -> anyhow::Result<Vec<serde_json::Map<String, Value>>> {
        let root = self.last_json()?;
        let Some(Value::Array(entries)) = root.get("imposters") else {
            bail!("no 'imposters' array in: {root}");
        };
        Ok(entries
            .iter()
            .filter_map(|e| e.as_object().cloned())
            .collect())
    }

    fn listed_named(&self, name: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
        let port = self.named_port(name)?;
        let listed = self.listed()?;
        let ports: Vec<u64> = listed.iter().filter_map(listed_port).collect();
        listed
            .into_iter()
            .find(|e| listed_port(e) == Some(u64::from(port)))
            .ok_or_else(|| {
                anyhow!("imposter '{name}' (port {port}) not listed; got ports {ports:?}")
            })
    }
}

fn listed_port(entry: &serde_json::Map<String, Value>) -> Option<u64> {
    entry.get("port").and_then(Value::as_u64)
}

fn names(csv: &str) -> Vec<String> {
    csv.split(',').map(|n| n.trim().to_string()).collect()
}

/// `raw` posted to `admin` at `port`: the status and body.
async fn post_at_port(
    admin: &str,
    raw: &str,
    port: u16,
    what: &str,
) -> anyhow::Result<(u16, String)> {
    let mut map = parse_object(raw, what)?;
    map.insert("port".to_string(), port.into());
    post_imposter(admin, &Value::Object(map).to_string()).await
}

/// `raw` posted at the highest free port below `ceiling` (and at or above `floor`), walking down
/// past ports the engine reports taken. `None` when the walk finds none.
async fn post_below(
    admin: &str,
    raw: &str,
    ceiling: u16,
    floor: u16,
    what: &str,
) -> anyhow::Result<Option<u16>> {
    for port in (floor..ceiling).rev().take(64) {
        let (status, body) = post_at_port(admin, raw, port, what).await?;
        if status == 201 {
            return Ok(Some(port));
        }
        if !port_taken(status, &body) {
            bail!("POST {what} at port {port} returned {status}, expected 201: {body}");
        }
    }
    Ok(None)
}

// #713: creating on the higher port FIRST is what makes an ascending listing a real claim rather
// than an echo of insertion order. The first takes a fresh port (engine-assigned, or the container
// pool's); the second the highest free one below it, bound by the engine, so no probe-then-bind.
#[given(
    regex = r#"^imposters "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)" from fixture "((?:\\.|[^"\\])*)" on descending ports$"#
)]
async fn imposters_on_descending_ports(
    w: &mut PlaneA,
    first: String,
    second: String,
    fixture: String,
) -> Step {
    let (first, second, fixture) = (unescape(&first), unescape(&second), unescape(&fixture));
    let h = harness();
    let raw = h.rewrite_fixture(&load_fixture(&fixture)?);
    let floor = h.imposter_port_pool().map_or(1024, |p| p.lo());
    for _ in 0..3 {
        let hi = post_at_engine_port(&h.admin_url, &raw, &format!("fixture {fixture}")).await?;
        w.tracked.push(hi);
        if let Some(lo) =
            post_below(&h.admin_url, &raw, hi, floor, &format!("fixture {fixture}")).await?
        {
            w.tracked.push(lo);
            w.named.insert(first, hi);
            w.named.insert(second, lo);
            w.current = Some(lo);
            return Ok(());
        }
        // No free port below this one (the pool's bottom): give it back and take another.
        delete_imposter(&h.admin_url, hi).await?;
    }
    Err(anyhow!("found no free port below a fresh one in 3 tries").into())
}

#[given(regex = r#"^a fresh imposter "((?:\\.|[^"\\])*)" from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn fresh_named_imposter(w: &mut PlaneA, name: String, fixture: String) -> Step {
    fresh_imposter(w, fixture).await?;
    let port = w.current()?;
    w.named.insert(unescape(&name), port);
    Ok(())
}

/// Each imposter at the port the isolated engine assigns. Not tracked: the engine and its imposters
/// are stopped after the scenario.
#[given(
    regex = r#"^an isolated engine with imposters "((?:\\.|[^"\\])*)" from "((?:\\.|[^"\\])*)" and "((?:\\.|[^"\\])*)" from "((?:\\.|[^"\\])*)"$"#
)]
async fn isolated_engine_with(
    w: &mut PlaneA,
    n1: String,
    f1: String,
    n2: String,
    f2: String,
) -> Step {
    if let Some(old) = w.isolated.take() {
        old.stop().await?;
    }
    let engine = harness().start_isolated_engine().await?;
    let admin = engine.admin_url.clone();
    w.isolated = Some(engine);
    for (name, fixture) in [(n1, f1), (n2, f2)] {
        let (name, fixture) = (unescape(&name), unescape(&fixture));
        let raw = harness().rewrite_fixture(&load_fixture(&fixture)?);
        let port = post_assigned(&admin, &raw, &format!("fixture {fixture}")).await?;
        w.named.insert(name, port);
    }
    Ok(())
}

#[then(regex = r#"^the imposter list shows "((?:\\.|[^"\\])*)" before "((?:\\.|[^"\\])*)"$"#)]
async fn list_shows_before(w: &mut PlaneA, a: String, b: String) -> Step {
    let (a, b) = (unescape(&a), unescape(&b));
    let (pa, pb) = (w.named_port(&a)?, w.named_port(&b)?);
    let ports: Vec<u64> = w.listed()?.iter().filter_map(listed_port).collect();
    let at = |p: u16| ports.iter().position(|&x| x == u64::from(p));
    w.ensure(
        matches!((at(pa), at(pb)), (Some(ia), Some(ib)) if ia < ib),
        || format!("expected '{a}' ({pa}) listed before '{b}' ({pb}), got ports {ports:?}"),
    )
}

#[then(regex = r#"^the listed imposters "((?:\\.|[^"\\])*)" all carry field "((?:\\.|[^"\\])*)"$"#)]
async fn listed_all_carry(w: &mut PlaneA, csv: String, field: String) -> Step {
    let field = unescape(&field);
    for name in names(&unescape(&csv)) {
        let entry = w.listed_named(&name)?;
        w.ensure(entry.contains_key(&field), || {
            format!(
                "listed '{name}' lacks '{field}': {}",
                Value::Object(entry.clone())
            )
        })?;
    }
    Ok(())
}

#[then(regex = r#"^the listed imposters "((?:\\.|[^"\\])*)" carry no field "((?:\\.|[^"\\])*)"$"#)]
async fn listed_carry_no(w: &mut PlaneA, csv: String, field: String) -> Step {
    let field = unescape(&field);
    for name in names(&unescape(&csv)) {
        let entry = w.listed_named(&name)?;
        w.ensure(!entry.contains_key(&field), || {
            format!(
                "listed '{name}' carries '{field}': {}",
                Value::Object(entry.clone())
            )
        })?;
    }
    Ok(())
}

#[then(regex = r#"^the listed imposter "((?:\\.|[^"\\])*)" has (\d+) stubs$"#)]
async fn listed_stub_count(w: &mut PlaneA, name: String, n: usize) -> Step {
    let name = unescape(&name);
    let entry = w.listed_named(&name)?;
    let got = entry.get("stubs").and_then(Value::as_array).map(Vec::len);
    w.ensure(got == Some(n), || {
        format!(
            "expected '{name}' to list {n} stubs, got {}: {}",
            got.map_or_else(|| "no stubs array".to_string(), |g| g.to_string()),
            Value::Object(entry.clone())
        )
    })
}

#[then(regex = r#"^the listed imposter "((?:\\.|[^"\\])*)" does not contain "((?:\\.|[^"\\])*)"$"#)]
async fn listed_lacks(w: &mut PlaneA, name: String, sub: String) -> Step {
    let (name, sub) = (unescape(&name), unescape(&sub));
    let json = Value::Object(w.listed_named(&name)?).to_string();
    w.ensure(!json.contains(&sub), || {
        format!("expected '{name}' without '{sub}', got {json}")
    })
}

#[then(regex = r#"^the response lists exactly the imposters "((?:\\.|[^"\\])*)"$"#)]
async fn lists_exactly(w: &mut PlaneA, csv: String) -> Step {
    let mut want: Vec<u64> = names(&unescape(&csv))
        .iter()
        .map(|n| w.named_port(n).map(u64::from))
        .collect::<anyhow::Result<_>>()?;
    want.sort_unstable();
    let mut got: Vec<u64> = w.listed()?.iter().filter_map(listed_port).collect();
    got.sort_unstable();
    w.ensure(got == want, || {
        format!("expected exactly ports {want:?}, got {got:?}")
    })
}

async fn current_accepts(w: &PlaneA) -> anyhow::Result<(&'static str, u16, bool)> {
    let port = w.current()?;
    let host = harness().imposter_host();
    let accepted = tokio::task::spawn_blocking(move || raw::tcp_connect_accepted(host, port))
        .await
        .context("the connect task")??;
    Ok((host, port, accepted))
}

// The positive control for the refusal below: the same bare connect, before the delete.
#[then(regex = r"^a TCP connect to the current imposter is accepted$")]
async fn tcp_connect_accepted(w: &mut PlaneA) -> Step {
    let (host, port, ok) = current_accepts(w).await?;
    w.ensure(ok, || {
        format!("connect to {host}:{port} was refused before any delete")
    })
}

// #596/#597: one attempt, no retry: the delete must already have unbound the listener.
#[then(regex = r"^an immediate TCP connect to the current imposter is refused$")]
async fn tcp_connect_refused(w: &mut PlaneA) -> Step {
    let (host, port, ok) = current_accepts(w).await?;
    w.ensure(!ok, || {
        format!("connect to {host}:{port} succeeded right after the delete")
    })
}

// Read while writing: past the cap the engine answers and closes mid-upload.
#[when(regex = r#"^the client posts (\d+) bytes to admin "((?:\\.|[^"\\])*)"$"#)]
async fn post_bytes_to_admin(w: &mut PlaneA, n: usize, path: String) -> Step {
    let url = format!("{}{}", harness().admin_url, unescape(&path));
    w.curl.push(format!(
        "head -c {n} /dev/zero | tr '\\0' a | curl -i -X POST {url} --data-binary @-"
    ));
    let headers = json_content_type();
    let started = Instant::now();
    let resp = tokio::task::spawn_blocking(move || {
        raw::send_oversize(&url, "POST", &headers, &vec![b'a'; n])
    })
    .await
    .context("the over-size send task")??;
    w.elapsed = Some(started.elapsed());
    w.last = Some(resp);
    Ok(())
}

// ── raw sockets (feature 03, #196; rift#1234) ────────────────────────────────────────────────

/// The admin listener's host and port, for the `/__rift/{port}` gateway.
fn admin_host_port() -> anyhow::Result<(String, u16)> {
    let url = reqwest::Url::parse(&harness().admin_url).context("the admin URL")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("admin URL {url} has no host"))?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("admin URL {url} has no port"))?;
    Ok((host, port))
}

async fn capture(
    w: &mut PlaneA,
    host: String,
    port: u16,
    method: String,
    path: String,
    tls: bool,
) -> Step {
    let host_header = host.clone();
    capture_with_host(w, host, port, method, path, host_header, tls).await
}

/// [`capture`] with a `Host` header other than the host connected to (the front door routes on it).
async fn capture_with_host(
    w: &mut PlaneA,
    host: String,
    port: u16,
    method: String,
    path: String,
    host_header: String,
    tls: bool,
) -> Step {
    w.curl.push(format!(
        "printf '{method} {path} HTTP/1.1\\r\\nHost: {host_header}\\r\\n\\r\\n' | {}",
        if tls {
            format!("openssl s_client -quiet -connect {host}:{port}")
        } else {
            format!("nc {host} {port}")
        }
    ));
    let cap = tokio::task::spawn_blocking(move || {
        raw::raw_capture(&host, port, &method, &path, &host_header, tls)
    })
    .await
    .context("the raw capture task")??;
    w.raw = Some(cap);
    Ok(())
}

#[when(regex = r#"^the client sends a raw "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)"$"#)]
async fn send_raw(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let host = harness().imposter_host().to_string();
    capture(w, host, port, unescape(&method), unescape(&path), false).await
}

#[when(regex = r#"^the client sends a raw "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" over TLS$"#)]
async fn send_raw_tls(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let host = harness().imposter_host().to_string();
    capture(w, host, port, unescape(&method), unescape(&path), true).await
}

#[when(
    regex = r#"^the client sends a raw "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" via the gateway$"#
)]
async fn send_raw_via_gateway(w: &mut PlaneA, method: String, path: String) -> Step {
    let port = w.current()?;
    let (host, admin_port) = admin_host_port()?;
    let path = format!("/__rift/{port}{}", unescape(&path));
    capture(w, host, admin_port, unescape(&method), path, false).await
}

impl PlaneA {
    fn raw(&self) -> anyhow::Result<&RawCapture> {
        self.raw
            .as_ref()
            .ok_or_else(|| anyhow!("no raw send yet in this scenario"))
    }
}

fn head_of(text: &str) -> String {
    text.chars().take(200).collect()
}

#[then(regex = r"^the raw response is empty$")]
async fn raw_empty(w: &mut PlaneA) -> Step {
    let c = w.raw()?;
    w.ensure(c.bytes.is_empty() && c.closed, || {
        format!(
            "expected the connection to close with no bytes, got {} (closed={}, {}): '{}'",
            c.bytes.len(),
            c.closed,
            c.error.as_deref().unwrap_or("EOF"),
            head_of(&c.text())
        )
    })
}

#[then(regex = r#"^the raw response starts with "((?:\\.|[^"\\])*)"$"#)]
async fn raw_starts_with(w: &mut PlaneA, prefix: String) -> Step {
    let prefix = unescape(&prefix);
    let text = w.raw()?.text();
    w.ensure(text.starts_with(&prefix), || {
        format!(
            "expected raw bytes to start with '{prefix}', got '{}'",
            head_of(&text)
        )
    })
}

#[then(regex = r#"^the raw response contains "((?:\\.|[^"\\])*)"$"#)]
async fn raw_contains(w: &mut PlaneA, sub: String) -> Step {
    let sub = unescape(&sub);
    let text = w.raw()?.text();
    w.ensure(text.contains(&sub), || {
        format!(
            "expected raw bytes to contain '{sub}', got '{}'",
            head_of(&text)
        )
    })
}

#[then(regex = r"^the TLS handshake completed$")]
async fn tls_handshake_completed(w: &mut PlaneA) -> Step {
    let c = w.raw()?;
    w.ensure(c.tls_handshake == Some(true), || {
        format!(
            "expected a completed TLS handshake, got {:?} ({})",
            c.tls_handshake,
            c.error.as_deref().unwrap_or("no error")
        )
    })
}

#[then(regex = r#"^the raw connection outcome is "((?:\\.|[^"\\])*)"$"#)]
async fn raw_outcome(w: &mut PlaneA, outcome: String) -> Step {
    let outcome = unescape(&outcome);
    let c = w.raw()?;
    let passed = c
        .shows(&outcome)
        .ok_or_else(|| anyhow!("unknown raw outcome '{outcome}'"))?;
    w.ensure(passed, || {
        let text: String = c.text().chars().take(120).collect();
        format!(
            "expected '{outcome}', got {} bytes (closed={}, {}): '{text}'",
            c.bytes.len(),
            c.closed,
            c.error.as_deref().unwrap_or("EOF")
        )
    })
}

#[when(
    regex = r#"^an h2c client sends "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)" on one connection via the gateway$"#
)]
async fn h2c_via_gateway(w: &mut PlaneA, first: String, second: String) -> Step {
    let port = w.current()?;
    let (host, admin_port) = admin_host_port()?;
    let first = format!("/__rift/{port}{}", unescape(&first));
    let second = format!("/__rift/{port}{}", unescape(&second));
    w.curl.push(format!(
        "curl -sv --http2-prior-knowledge -H 'Host: {host}' http://{host}:{admin_port}{first} http://{host}:{admin_port}{second}"
    ));
    let streams = tokio::task::spawn_blocking(move || {
        raw::h2c_two_streams(&host, admin_port, &host, &first, &second)
    })
    .await
    .context("the h2c task")??;
    w.h2c = Some(streams);
    Ok(())
}

impl PlaneA {
    fn h2c(&self) -> anyhow::Result<&(H2Outcome, H2Outcome)> {
        self.h2c
            .as_ref()
            .ok_or_else(|| anyhow!("no h2c exchange yet in this scenario"))
    }
}

#[then(regex = r"^the first HTTP/2 stream was reset with INTERNAL_ERROR$")]
async fn first_stream_reset(w: &mut PlaneA) -> Step {
    let (first, _) = w.h2c()?;
    w.ensure(*first == H2Outcome::Reset(H2_INTERNAL_ERROR), || {
        format!("expected RST_STREAM INTERNAL_ERROR on the first stream, got {first:?}")
    })
}

#[then(regex = r#"^the second HTTP/2 stream answered (\d+) "((?:\\.|[^"\\])*)"$"#)]
async fn second_stream_answered(w: &mut PlaneA, status: u16, body: String) -> Step {
    let want = H2Outcome::Answered {
        status: Some(status),
        body: unescape(&body),
    };
    let (_, second) = w.h2c()?;
    w.ensure(*second == want, || {
        format!("expected the same connection's next stream to answer {want:?}, got {second:?}")
    })
}

// ── error envelope, continued (feature 13) ───────────────────────────────────────────────────

// Pinned per door, not derived from the status: the four `inject` doors carry a Mountebank slug
// ("invalid injection", …) where every other door carries the status as a string.
#[then(regex = r#"^the error envelope code is "((?:\\.|[^"\\])*)"$"#)]
async fn envelope_code_is(w: &mut PlaneA, code: String) -> Step {
    let code = unescape(&code);
    let entries = w.envelope()?;
    w.ensure(entries.iter().any(|e| e.code == code), || {
        let got: Vec<&str> = entries.iter().map(|e| e.code.as_str()).collect();
        format!("expected envelope code '{code}', got {}", got.join(", "))
    })
}

// The negative: a door that deliberately keeps a non-JSON reply must not be swept into the
// envelope by a later change.
#[then(regex = r"^the response is not a JSON error envelope$")]
async fn response_is_not_envelope(w: &mut PlaneA) -> Step {
    let body = w.last()?.body.clone();
    w.ensure(parse_error_envelope(&body).is_err(), || {
        format!("expected a non-envelope reply, but the body parsed as one: '{body}'")
    })
}

// Generated rather than inlined: an over-the-cap literal has no place in a feature file.
#[when(regex = r#"^the request body is (\d+) MiB of "((?:\\.|[^"\\])*)"$"#)]
async fn request_body_mib(w: &mut PlaneA, mib: usize, ch: String) {
    w.pending.body = Some(unescape(&ch).repeat(mib * 1024 * 1024));
}

/// An unresolvable script reference (`kind` is the field to poison: `ref` or `file`) is refused at
/// creation, on the admin plane. The response is kept for the status/envelope steps; whatever port
/// a regression that accepts it is given is tracked, so the stray imposter cannot squat it.
#[when(
    regex = r#"^the client posts an imposter whose script ("(?:\\.|[^"\\])*"|\S+) is unresolvable$"#
)]
async fn post_unresolvable_script(w: &mut PlaneA, kind: String) -> Step {
    let kind = flexible_string(&kind);
    let value = if kind == "file" {
        "no-such-script.rhai"
    } else {
        "no-such-script"
    };
    let mut script = serde_json::Map::new();
    script.insert("engine".to_string(), "rhai".into());
    script.insert(kind, value.into());
    let mut imposter = serde_json::json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"path": "/x"}}],
            "responses": [{"_rift": {"script": script}}]
        }]
    });
    if let Some(pool) = harness().imposter_port_pool() {
        let port = pool.next_port();
        w.tracked.push(port);
        imposter["port"] = port.into();
    }
    let url = format!("{}/imposters", w.admin_url());
    let json = imposter.to_string();
    w.send_with(
        &clients().verifying,
        "POST",
        &url,
        &json_content_type(),
        Some(&json),
    )
    .await?;
    let resp = w.last()?;
    if resp.status == 201 {
        let port = declared_port(&resp.body, "the 201 for an unresolvable script")?;
        w.tracked.push(port);
    }
    Ok(())
}

// ── scripting (feature 04) ───────────────────────────────────────────────────────────────────

// Real time: a `flowState.ttlSeconds` key has to expire.
#[when(regex = r"^the client waits (\d+) ms$")]
async fn client_waits(_w: &mut PlaneA, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[then(regex = r"^the engine remains responsive$")]
async fn engine_responsive(w: &mut PlaneA) -> Step {
    let url = format!("{}/imposters", harness().admin_url);
    let status = get(&url).await?.status;
    w.ensure(status == 200, || {
        format!("admin API not responsive: GET /imposters -> {status}")
    })
}

// ── admin surface (feature 08) ───────────────────────────────────────────────────────────────

/// Java's `URLEncoder.encode` (the Scala runner's): form encoding, a space as `+`.
fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                char::from(b).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `?match=header:h=v&…` over `pairs`, or nothing.
fn header_match_query(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let clauses: Vec<String> = pairs
        .iter()
        .map(|(h, v)| format!("match=header:{}={}", form_encode(h), form_encode(v)))
        .collect();
    format!("?{}", clauses.join("&"))
}

impl PlaneA {
    /// `{admin}/imposters/{current}{rest}`.
    fn current_admin_url(&self, rest: &str) -> anyhow::Result<String> {
        Ok(format!(
            "{}/imposters/{}{rest}",
            self.admin_url(),
            self.current()?
        ))
    }

    /// An admin request whose answer a step asserts itself: recorded for the repro, not kept as
    /// the last response.
    async fn admin_call(
        &mut self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> anyhow::Result<Response> {
        let headers = if body.is_some() {
            json_content_type()
        } else {
            Vec::new()
        };
        self.record_curl(method, url, &headers, body);
        let (resp, _) = exchange(&clients().verifying, method, url, &headers, body)
            .await?
            .with_context(|| format!("transport failure on {method} {url}"))?;
        Ok(resp)
    }

    /// The same, failing the step unless the engine answers `want`.
    async fn admin_expect(
        &mut self,
        method: &str,
        url: &str,
        body: Option<&str>,
        want: u16,
    ) -> anyhow::Result<Response> {
        let resp = self.admin_call(method, url, body).await?;
        self.ensure(resp.status == want, || {
            format!(
                "{method} {url} -> {} (expected {want}): {}",
                resp.status, resp.body
            )
        })
        .map_err(|f| f.0)?;
        Ok(resp)
    }

    async fn stub_by_id(&mut self, id: &str) -> anyhow::Result<Response> {
        let url = self.current_admin_url(&format!("/stubs/by-id/{id}"))?;
        self.admin_call("GET", &url, None).await
    }

    /// How many recorded requests `GET .../requests` lists under the header `pairs`: a bare
    /// array, or `{"requests": [...]}`.
    async fn recorded_count(&mut self, pairs: &[(&str, &str)]) -> anyhow::Result<usize> {
        let url = self.current_admin_url(&format!("/requests{}", header_match_query(pairs)))?;
        let resp = self.admin_expect("GET", &url, None, 200).await?;
        let json: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
        match &json {
            Value::Array(es) => Ok(es.len()),
            Value::Object(m) => match m.get("requests") {
                Some(Value::Array(es)) => Ok(es.len()),
                _ => bail!("no 'requests' array in response: {}", resp.body),
            },
            _ => bail!("/requests response is not an array/object: {}", resp.body),
        }
    }

    /// `POST .../verify` with one `equals`-on-path predicate (rift#494): the engine counts its own
    /// recorded requests.
    async fn server_verify(
        &mut self,
        path: &str,
        include_closest: bool,
        flow: Option<&str>,
    ) -> anyhow::Result<Verified> {
        let mut body = serde_json::json!({"predicates": [{"equals": {"path": path}}]});
        if include_closest {
            body["includeClosest"] = Value::Bool(true);
        }
        if let Some(flow) = flow {
            body["flowId"] = flow.into();
        }
        let url = self.current_admin_url("/verify")?;
        let resp = self
            .admin_expect("POST", &url, Some(&body.to_string()), 200)
            .await?;
        let json: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
        let closest = json.get("closest");
        Ok(Verified {
            matched: json.get("matched").and_then(Value::as_i64),
            total: json.get("total").and_then(Value::as_i64),
            closest_path: closest
                .and_then(|c| c.get("request"))
                .and_then(|r| r.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string),
            failed_predicates: closest
                .and_then(|c| c.get("failedPredicates"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        })
    }
}

/// A verify's counts; a count the engine left out is `None`, which never equals an expectation.
struct Verified {
    matched: Option<i64>,
    total: Option<i64>,
    closest_path: Option<String>,
    failed_predicates: usize,
}

fn count(n: Option<i64>) -> String {
    n.map_or_else(|| "<absent>".to_string(), |n| n.to_string())
}

#[then(regex = r#"^the stub with id "((?:\\.|[^"\\])*)" is retrievable$"#)]
async fn stub_retrievable(w: &mut PlaneA, id: String) -> Step {
    let id = unescape(&id);
    let resp = w.stub_by_id(&id).await?;
    w.ensure(resp.status == 200, || {
        format!(
            "stubs/by-id/{id} -> {} (expected 200): {}",
            resp.status, resp.body
        )
    })
}

#[then(regex = r#"^the stub with id "((?:\\.|[^"\\])*)" includes "((?:\\.|[^"\\])*)"$"#)]
async fn stub_includes(w: &mut PlaneA, id: String, sub: String) -> Step {
    let (id, sub) = (unescape(&id), unescape(&sub));
    let resp = w.stub_by_id(&id).await?;
    w.ensure(resp.status == 200 && resp.body.contains(&sub), || {
        format!(
            "stubs/by-id/{id} (status {}) did not include '{sub}': {}",
            resp.status, resp.body
        )
    })
}

#[then(regex = r#"^no stub has id "((?:\\.|[^"\\])*)"$"#)]
async fn no_stub_with_id(w: &mut PlaneA, id: String) -> Step {
    let id = unescape(&id);
    let resp = w.stub_by_id(&id).await?;
    w.ensure(resp.status == 404, || {
        format!(
            "stubs/by-id/{id} -> {} (expected 404): {}",
            resp.status, resp.body
        )
    })
}

async fn set_enabled(w: &mut PlaneA, enabled: bool) -> Step {
    let verb = if enabled { "enable" } else { "disable" };
    let url = w.current_admin_url(&format!("/{verb}"))?;
    w.admin_expect("POST", &url, None, 200).await?;
    // The Scala runner's settle, from the manual conformance script's `sleep 0.1`.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

#[when(regex = r"^the client disables the imposter$")]
async fn disable_imposter(w: &mut PlaneA) -> Step {
    set_enabled(w, false).await
}

#[when(regex = r"^the client enables the imposter$")]
async fn enable_imposter(w: &mut PlaneA) -> Step {
    set_enabled(w, true).await
}

#[then(regex = r"^recorded requests total (\d+)$")]
async fn recorded_total(w: &mut PlaneA, n: usize) -> Step {
    let got = w.recorded_count(&[]).await?;
    w.ensure(got == n, || {
        format!("expected {n} recorded request(s), got {got}")
    })
}

#[then(
    regex = r#"^recorded requests matching header "((?:\\.|[^"\\])*)" value "((?:\\.|[^"\\])*)" total (\d+)$"#
)]
async fn recorded_matching(w: &mut PlaneA, h: String, v: String, n: usize) -> Step {
    let (h, v) = (unescape(&h), unescape(&v));
    let got = w.recorded_count(&[(&h, &v)]).await?;
    w.ensure(got == n, || {
        format!("expected {n} request(s) matching header {h}={v}, got {got}")
    })
}

#[then(
    regex = r#"^recorded requests matching header "((?:\\.|[^"\\])*)" value "((?:\\.|[^"\\])*)" and header "((?:\\.|[^"\\])*)" value "((?:\\.|[^"\\])*)" total (\d+)$"#
)]
async fn recorded_matching_both(
    w: &mut PlaneA,
    h1: String,
    v1: String,
    h2: String,
    v2: String,
    n: usize,
) -> Step {
    let (h1, v1, h2, v2) = (unescape(&h1), unescape(&v1), unescape(&h2), unescape(&v2));
    let got = w.recorded_count(&[(&h1, &v1), (&h2, &v2)]).await?;
    w.ensure(got == n, || {
        format!("expected {n} request(s) matching header {h1}={v1} and {h2}={v2}, got {got}")
    })
}

#[when(
    regex = r#"^the client deletes recorded requests matching header "((?:\\.|[^"\\])*)" value "((?:\\.|[^"\\])*)"$"#
)]
async fn delete_recorded_matching(w: &mut PlaneA, h: String, v: String) -> Step {
    let (h, v) = (unescape(&h), unescape(&v));
    let url = w.current_admin_url(&format!("/requests{}", header_match_query(&[(&h, &v)])))?;
    w.admin_expect("DELETE", &url, None, 200).await?;
    Ok(())
}

#[then(
    regex = r#"^a verify for path "((?:\\.|[^"\\])*)" matches (\d+) of (\d+) recorded requests$"#
)]
async fn verify_matches(w: &mut PlaneA, path: String, matched: i64, total: i64) -> Step {
    let path = unescape(&path);
    let r = w.server_verify(&path, false, None).await?;
    w.ensure(r.matched == Some(matched) && r.total == Some(total), || {
        format!(
            "verify path={path}: expected matched={matched} total={total}, got matched={} total={}",
            count(r.matched),
            count(r.total)
        )
    })
}

#[then(
    regex = r#"^a verify for path "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)" matches (\d+) of (\d+) recorded requests$"#
)]
async fn verify_in_flow_matches(
    w: &mut PlaneA,
    path: String,
    flow: String,
    matched: i64,
    total: i64,
) -> Step {
    let (path, flow) = (unescape(&path), unescape(&flow));
    let r = w.server_verify(&path, false, Some(&flow)).await?;
    w.ensure(r.matched == Some(matched) && r.total == Some(total), || {
        format!(
            "verify path={path} flowId={flow}: expected matched={matched} total={total}, got matched={} total={}",
            count(r.matched),
            count(r.total)
        )
    })
}

#[then(
    regex = r#"^a verify for path "((?:\\.|[^"\\])*)" with includeClosest reports the closest request path "((?:\\.|[^"\\])*)"$"#
)]
async fn verify_closest(w: &mut PlaneA, path: String, closest: String) -> Step {
    let (path, closest) = (unescape(&path), unescape(&closest));
    let r = w.server_verify(&path, true, None).await?;
    w.ensure(
        r.matched == Some(0)
            && r.closest_path.as_deref() == Some(closest.as_str())
            && r.failed_predicates > 0,
        || {
            format!(
                "verify path={path} includeClosest: expected matched=0, closest path={closest}, \
                 failedPredicates>0; got matched={}, closest={:?}, failed={}",
                count(r.matched),
                r.closest_path,
                r.failed_predicates
            )
        },
    )
}

#[when(
    regex = r#"^the client deletes flow-state "((?:\\.|[^"\\])*)" in flow "((?:\\.|[^"\\])*)"$"#
)]
async fn delete_flow_state(w: &mut PlaneA, key: String, flow: String) -> Step {
    let url = w.flow_state_url(&unescape(&flow), &unescape(&key))?;
    w.admin_expect("DELETE", &url, None, 200).await?;
    Ok(())
}

/// The `port`s a `{"imposters": [...]}` batch declares.
fn batch_ports(raw: &str, what: &str) -> anyhow::Result<Vec<u16>> {
    let root = parse_object(raw, what)?;
    let Some(Value::Array(imposters)) = root.get("imposters") else {
        bail!("{what} has no top-level 'imposters' array");
    };
    imposters
        .iter()
        .filter_map(|i| i.get("port").and_then(Value::as_u64))
        .map(|p| u16::try_from(p).with_context(|| format!("{what} port {p}")))
        .collect()
}

/// `PUT {admin}/imposters`: the atomic validate-then-swap of the whole imposter set.
async fn replace_all(w: &mut PlaneA, raw: &str, what: &str) -> anyhow::Result<()> {
    let url = format!("{}/imposters", w.admin_url());
    w.admin_expect("PUT", &url, Some(raw), 200)
        .await
        .with_context(|| format!("PUT /imposters ({what})"))?;
    Ok(())
}

#[when(regex = r#"^the client replaces all imposters with fixture "((?:\\.|[^"\\])*)"$"#)]
async fn replace_all_with_fixture(w: &mut PlaneA, name: String) -> Step {
    let name = unescape(&name);
    let raw = harness().rewrite_fixture(&load_fixture(&name)?);
    let what = format!("fixture {name}");
    let ports = batch_ports(&raw, &what)?;
    replace_all(w, &raw, &what).await?;
    w.tracked.extend(ports);
    Ok(())
}

/// A response `_rift.script` whose rhai does not compile: the whole batch must be refused, so its
/// port is never bound. The Scala runner's `invalidImposterBatch`, byte for byte.
const INVALID_IMPOSTER_BATCH: &str = r#"{"imposters":[{"port":4810,"protocol":"http","stubs":[{"predicates":[{"equals":{"path":"/x"}}],"responses":[{"_rift":{"script":{"engine":"rhai","code":"fn respond(ctx) { broken @#$ syntax"}}}]}]}]}"#;

#[when(regex = r"^the client replaces all imposters with an invalid batch$")]
async fn replace_all_invalid(w: &mut PlaneA) -> Step {
    let url = format!("{}/imposters", w.admin_url());
    let resp = w
        .admin_call("PUT", &url, Some(INVALID_IMPOSTER_BATCH))
        .await?;
    // A regression that applies it must not leave 4810 behind for later scenarios.
    if resp.status < 300 {
        w.tracked.push(4810);
    }
    w.ensure(resp.status == 400, || {
        format!(
            "PUT /imposters (invalid batch) -> {} (expected 400): {}",
            resp.status, resp.body
        )
    })
}

#[then(regex = r#"^the response JSON has (\d+) stubs and none carries "((?:\\.|[^"\\])*)"$"#)]
async fn stubs_none_carry(w: &mut PlaneA, n: usize, field: String) -> Step {
    let field = unescape(&field);
    let root = w.last_json()?;
    let Value::Object(fields) = &root else {
        return Err(anyhow!("response is not a JSON object: {}", w.last()?.body).into());
    };
    let stubs = fields.get("stubs").and_then(Value::as_array);
    let carrying = stubs.map_or(0, |s| {
        s.iter()
            .filter(|stub| stub.as_object().is_some_and(|o| o.contains_key(&field)))
            .count()
    });
    w.ensure(stubs.is_some_and(|s| s.len() == n) && carrying == 0, || {
        format!(
            "expected {n} stubs with no '{field}', got {} stubs, {carrying} carrying it",
            stubs.map_or_else(|| "no".to_string(), |s| s.len().to_string())
        )
    })
}

#[then(
    regex = r#"^every element of the response JSON array has a non-negative integer "((?:\\.|[^"\\])*)"$"#
)]
async fn every_element_non_negative_integer(w: &mut PlaneA, field: String) -> Step {
    let field = unescape(&field);
    let Value::Array(es) = w.last_json()? else {
        return Err(anyhow!("response is not a JSON array: {}", w.last()?.body).into());
    };
    let whole_non_negative = |v: &Value| match v {
        Value::Number(n) => {
            n.as_u64().is_some() || n.as_f64().is_some_and(|f| f >= 0.0 && f.fract() == 0.0)
        }
        _ => false,
    };
    let bad: Vec<&Value> = es
        .iter()
        .filter(|e| !json_at(e, &field).is_some_and(whole_non_negative))
        .collect();
    w.ensure(!es.is_empty() && bad.is_empty(), || {
        format!(
            "expected every element to carry a non-negative integer '{field}'; offending: {} (of {})",
            Value::Array(bad.into_iter().cloned().collect()),
            es.len()
        )
    })
}

// Two header lines of one name: the staged request holds one per name.
#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" with header "((?:\\.|[^"\\])*)" repeated as "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)"$"#
)]
async fn send_repeated_header(
    w: &mut PlaneA,
    method: String,
    path: String,
    name: String,
    v1: String,
    v2: String,
) -> Step {
    let (method, name) = (unescape(&method), unescape(&name));
    let url = format!(
        "http://{}:{}{}",
        harness().imposter_host(),
        w.current()?,
        unescape(&path)
    );
    let headers = vec![(name.clone(), unescape(&v1)), (name, unescape(&v2))];
    w.record_curl(&method, &url, &headers, None);
    // `exchange` adds each pair with `RequestBuilder::header`, which appends.
    let (resp, _) = exchange(&clients().verifying, &method, &url, &headers, None)
        .await?
        .with_context(|| format!("transport failure on {method} {url}"))?;
    w.ensure(resp.status == 200, || {
        format!("{method} {url} -> {} (expected 200)", resp.status)
    })
}

/// `hex` is space-separated bytes (`ff fe`): a body that is not UTF-8, so the journal must record
/// it as base64 with `_mode: "binary"`.
#[when(regex = r#"^the client posts the bytes "((?:\\.|[^"\\])*)" to "((?:\\.|[^"\\])*)"$"#)]
async fn post_bytes(w: &mut PlaneA, hex: String, path: String) -> Step {
    let hex = unescape(&hex);
    let bytes: Vec<u8> = hex
        .split_whitespace()
        .map(|b| u8::from_str_radix(b, 16))
        .collect::<Result<_, _>>()
        .with_context(|| format!("bad hex bytes '{hex}'"))?;
    let url = format!(
        "http://{}:{}{}",
        harness().imposter_host(),
        w.current()?,
        unescape(&path)
    );
    let escaped: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
    w.curl.push(format!(
        "printf '{escaped}' | curl -X POST -H 'Content-Type: application/octet-stream' --data-binary @- '{url}'"
    ));
    let resp = clients()
        .verifying
        .post(&url)
        .header("Content-Type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    w.ensure(status == 200, || {
        format!("POST {url} -> {status} (expected 200)")
    })
}

#[when(regex = r#"^the client creates a port-less imposter from fixture "((?:\\.|[^"\\])*)"$"#)]
async fn create_port_less(w: &mut PlaneA, name: String) -> Step {
    let name = unescape(&name);
    let raw = harness().rewrite_fixture(&load_fixture(&name)?);
    let url = format!("{}/imposters", w.admin_url());
    let resp = w.admin_expect("POST", &url, Some(&raw), 201).await?;
    let port = declared_port(&resp.body, "the create response")
        .with_context(|| format!("no assigned 'port' in the create response: {}", resp.body))?;
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

#[when(
    regex = r#"^the client replaces all imposters with port-less batch fixture "((?:\\.|[^"\\])*)"$"#
)]
async fn replace_all_port_less(w: &mut PlaneA, name: String) -> Step {
    let name = unescape(&name);
    let raw = harness().rewrite_fixture(&load_fixture(&name)?);
    let what = format!("fixture {name}");
    let declared = batch_ports(&raw, &what)?;
    replace_all(w, &raw, &what).await?;
    // Declared ports first, so a failure below still tears them down.
    w.tracked.extend(declared.iter().copied());
    let listed = get(&format!("{}/imposters", w.admin_url())).await?;
    let json: Value = serde_json::from_str(&listed.body)
        .with_context(|| format!("invalid admin JSON (body={})", listed.body))?;
    let ports: Vec<u16> = json
        .get("imposters")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no 'imposters' array in: {}", listed.body))?
        .iter()
        .filter_map(|i| i.get("port").and_then(Value::as_u64))
        .map(|p| u16::try_from(p).with_context(|| format!("listed port {p}")))
        .collect::<anyhow::Result<_>>()?;
    w.tracked.extend(ports.iter().copied());
    // The OS picks an auto-assigned port (#1501): it is the one no fixture declared.
    let auto: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|p| !declared.contains(p))
        .collect();
    w.ensure(auto.len() == 1, || {
        format!("expected one auto-assigned port, got imposters {ports:?}")
    })?;
    w.current = auto.first().copied();
    Ok(())
}

/// The OS picks an auto-assigned port (#1501), so all that can be asserted is that one was given
/// and that no other imposter the scenario created holds it.
#[then(regex = r"^the assigned port is a non-zero port that no explicit fixture uses$")]
async fn assigned_port_is_auto(w: &mut PlaneA) -> Step {
    let port = w.current()?;
    let others = w.tracked.iter().filter(|p| **p == port).count();
    w.ensure(port != 0 && others == 1, || {
        format!(
            "expected an OS-assigned port held by no fixture, got {port} (tracked {:?})",
            w.tracked
        )
    })
}

// ── intercept / TLS-MITM forward proxy (feature 12, rift#413) ────────────────────────────────

/// The password the shared intercept truststore is exported under.
const TRUST_PASSWORD: &str = "changeit";
/// The `--intercept-auth` credential of the `@intercept-auth` engine.
const INTERCEPT_AUTH: &str = "u:p";

/// A listener `POST /intercept` started on the `@intercept-runtime` engine: its port, and the CA
/// PEM the 201 handed back (`returnCaKey`).
#[derive(Debug)]
struct RuntimeListener {
    port: u16,
    returned_ca: Option<String>,
}

/// An admin request whose answer the step keeps or asserts itself. `Content-Type: application/json`
/// on every one, as the Scala runner's `adminSend` sent it.
async fn intercept_admin(
    w: &mut PlaneA,
    admin: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> anyhow::Result<Response> {
    let url = format!("{admin}{path}");
    let headers = json_content_type();
    w.record_curl(method, &url, &headers, body);
    let (resp, _) = exchange(&clients().verifying, method, &url, &headers, body)
        .await?
        .with_context(|| format!("transport failure on {method} {url}"))?;
    Ok(resp)
}

/// `GET url`'s status, headers and raw body bytes (a truststore is binary).
async fn get_bytes(url: &str) -> anyhow::Result<(u16, reqwest::header::HeaderMap, Vec<u8>)> {
    let resp = clients()
        .verifying
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .bytes()
        .await
        .with_context(|| format!("reading GET {url}"))?;
    Ok((status, headers, body.to_vec()))
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

/// Add one rule to the shared listener, requiring a 201. Rules accumulate on the shared listener,
/// so the scenario is marked for the rule clear after it before the rule goes out.
async fn add_shared_rule(w: &mut PlaneA, rule: &Value) -> Step {
    w.intercept_rules_added = true;
    let admin = harness().admin_url.clone();
    let body = rule.to_string();
    let r = intercept_admin(w, &admin, "POST", "/intercept/rules", Some(&body)).await?;
    w.ensure(r.status == 201, || {
        format!(
            "POST /intercept/rules -> {} (expected 201): {}",
            r.status, r.body
        )
    })
}

/// A serve rule for any path on `host` answering 200 with `body`.
fn serve_rule(host: &str, body: &str) -> Value {
    serde_json::json!({"host": host, "action": {"serve": {"statusCode": 200, "body": body}}})
}

/// What a client trusting the shared listener's exported `truststore.p12` trusts.
async fn shared_truststore_roots() -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>>
{
    let url = format!(
        "{}/intercept/truststore.p12?password={TRUST_PASSWORD}",
        harness().admin_url
    );
    let (status, _, bytes) = get_bytes(&url).await?;
    if status != 200 {
        bail!("GET {url} -> {status} (expected 200)");
    }
    intercept::pkcs12_certificates(&bytes, TRUST_PASSWORD)
}

fn test_ca_roots() -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    tls::certs_from_pem(test_ca_pem()?.as_bytes())
}

/// The scenario's open raw tunnel to `host` through the shared listener, or a fresh one trusting
/// its exported truststore. Every raw step to the same host rides it, so a second exchange proves
/// the tunnel was kept alive.
async fn tunnel_to<'a>(w: &'a mut PlaneA, host: &str) -> anyhow::Result<&'a mut RawConnection> {
    if w.tunnel.as_ref().is_none_or(|(h, _)| h != host) {
        let port = harness().intercept_port;
        let proxy = harness().imposter_host();
        w.curl.push(format!(
            "curl -sv --http1.1 --proxy http://{proxy}:{port} --cacert <truststore CA> https://{host}/"
        ));
        let roots = shared_truststore_roots().await?;
        let conn =
            intercept::tunnel(proxy, port, host, 443, &roots, tls::ALPN_HTTP1, &[], None).await?;
        w.tunnel = Some((host.to_string(), conn));
    }
    match w.tunnel.as_mut() {
        Some((_, conn)) => Ok(conn),
        None => bail!("no raw tunnel to {host}"),
    }
}

/// The scenario's tunnel to `host`, which must already be open: a step using it proves reuse.
fn open_tunnel_to<'a>(w: &'a mut PlaneA, host: &str) -> anyhow::Result<&'a mut RawConnection> {
    match w.tunnel.as_mut() {
        Some((h, conn)) if h == host => Ok(conn),
        Some((h, _)) => bail!("no open raw tunnel to {host} (have one to {h})"),
        None => bail!("no open raw tunnel to {host}"),
    }
}

impl PlaneA {
    /// Keep a raw answer as the last response; a close before any answer fails the step.
    fn keep_raw(&mut self, read: RawRead, host: &str) -> Step {
        match read {
            RawRead::Response(r) => {
                self.last = Some(r.to_response());
                self.raw_response = Some(r);
                Ok(())
            }
            RawRead::Closed => {
                Err(anyhow!("the intercept connection for {host} closed without answering").into())
            }
        }
    }

    fn raw_response(&self) -> anyhow::Result<&wire::RawResponse> {
        self.raw_response
            .as_ref()
            .ok_or_else(|| anyhow!("no raw tunnel response yet in this scenario"))
    }
}

/// Send `request` on the shared tunnel to `host` and keep the answer.
async fn raw_exchange(w: &mut PlaneA, host: &str, request: &[u8]) -> Step {
    let read = tunnel_to(w, host).await?.exchange(request).await?;
    w.keep_raw(read, host)
}

/// `GET path` on `conn`, kept: a tunnel the scenario does not reuse.
async fn raw_get_on(w: &mut PlaneA, mut conn: RawConnection, host: &str, path: &str) -> Step {
    let read = conn
        .exchange(&wire::request("GET", host, path, &[], b""))
        .await?;
    w.keep_raw(read, host)
}

async fn ca_pem_of(w: &mut PlaneA, admin: &str) -> anyhow::Result<Response> {
    intercept_admin(w, admin, "GET", "/intercept/ca.pem", None).await
}

#[then(regex = r"^the intercept CA PEM is exported$")]
async fn intercept_ca_exported(w: &mut PlaneA) -> Step {
    let admin = harness().admin_url.clone();
    let r = ca_pem_of(w, &admin).await?;
    w.ensure(
        r.status == 200 && r.body.contains("BEGIN CERTIFICATE"),
        || {
            let head: String = r.body.chars().take(40).collect();
            format!("GET /intercept/ca.pem -> {}, body head='{head}'", r.status)
        },
    )
}

#[then(regex = r#"^the intercept truststore is exported with password "((?:\\.|[^"\\])*)"$"#)]
async fn intercept_truststore_exported(w: &mut PlaneA, password: String) -> Step {
    let password = unescape(&password);
    let url = format!(
        "{}/intercept/truststore.p12?password={password}",
        harness().admin_url
    );
    w.record_curl("GET", &url, &[], None);
    let (status, headers, body) = get_bytes(&url).await?;
    let echoed = header_str(&headers, "x-truststore-password");
    w.ensure(
        status == 200 && !body.is_empty() && echoed == password,
        || {
            format!(
                "truststore.p12 -> {status}, {} bytes, x-truststore-password='{echoed}' (expected '{password}')",
                body.len()
            )
        },
    )
}

#[then(regex = r"^the exported intercept CA is the caller-provided test CA$")]
async fn intercept_ca_is_test_ca(w: &mut PlaneA) -> Step {
    let expected = test_ca_pem()?;
    let admin = harness().admin_url.clone();
    let r = ca_pem_of(w, &admin).await?;
    w.ensure(
        r.status == 200 && r.body.trim() == expected.trim(),
        || {
            format!(
                "exported /intercept/ca.pem ({}) should byte-equal the provided test CA, proving the engine loaded the caller CA rather than minting an ephemeral one",
                r.status
            )
        },
    )
}

#[given(
    regex = r#"^an intercept rule serves (\d+) "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)" path "((?:\\.|[^"\\])*)"$"#
)]
async fn intercept_rule_serves_path(
    w: &mut PlaneA,
    status: u16,
    body: String,
    host: String,
    path: String,
) -> Step {
    let rule = serde_json::json!({
        "host": unescape(&host),
        "predicates": [{"equals": {"path": unescape(&path)}}],
        "action": {"serve": {"statusCode": status, "headers": {"content-type": "text/plain"}, "body": unescape(&body)}}
    });
    add_shared_rule(w, &rule).await
}

#[given(regex = r#"^an intercept rule forwards host "((?:\\.|[^"\\])*)" to the current imposter$"#)]
async fn intercept_rule_forwards(w: &mut PlaneA, host: String) -> Step {
    let port = w.current()?;
    let rule = serde_json::json!({"host": unescape(&host), "action": {"forward": {"port": port}}});
    add_shared_rule(w, &rule).await
}

#[given(
    regex = r#"^an intercept rule serves (\d+) "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)" when the body equals "((?:\\.|[^"\\])*)"$"#
)]
async fn intercept_rule_on_body(
    w: &mut PlaneA,
    status: u16,
    body: String,
    host: String,
    expected: String,
) -> Step {
    let rule = serde_json::json!({
        "host": unescape(&host),
        "predicates": [{"equals": {"body": unescape(&expected)}}],
        "action": {"serve": {"statusCode": status, "body": unescape(&body)}}
    });
    add_shared_rule(w, &rule).await
}

#[given(
    regex = r#"^an intercept rule serves (\d+) "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)" when header "((?:\\.|[^"\\])*)" equals "((?:\\.|[^"\\])*)"$"#
)]
async fn intercept_rule_on_header(
    w: &mut PlaneA,
    status: u16,
    body: String,
    host: String,
    header: String,
    value: String,
) -> Step {
    let mut headers = serde_json::Map::new();
    headers.insert(unescape(&header), unescape(&value).into());
    let rule = serde_json::json!({
        "host": unescape(&host),
        "predicates": [{"equals": {"headers": headers}}],
        "action": {"serve": {"statusCode": status, "body": unescape(&body)}}
    });
    add_shared_rule(w, &rule).await
}

/// A GET through the shared listener trusting its exported truststore, offering `h2` then
/// `http/1.1` as an HTTP/2-preferring client does. The version ALPN settled on is kept.
async fn https_via_intercept(w: &mut PlaneA, host: &str, path: &str) -> Step {
    let port = harness().intercept_port;
    let proxy = harness().imposter_host();
    w.curl.push(format!(
        "curl -sv --proxy http://{proxy}:{port} --cacert <truststore CA> https://{host}{path}"
    ));
    let roots = shared_truststore_roots().await?;
    let mut conn = intercept::tunnel(
        proxy,
        port,
        host,
        443,
        &roots,
        tls::ALPN_H2_FIRST,
        &[],
        None,
    )
    .await?;
    let (response, version) = if conn.alpn() == Some(b"h2".as_slice()) {
        match conn.h2_get(path, host).await? {
            H2Outcome::Answered {
                status: Some(status),
                body,
            } => (
                Response {
                    status,
                    body,
                    headers: BTreeMap::new(),
                },
                "HTTP_2",
            ),
            other => {
                return Err(anyhow!("the HTTP/2 GET {path} did not complete: {other:?}").into());
            }
        }
    } else {
        match conn
            .exchange(&wire::request("GET", host, path, &[], b""))
            .await?
        {
            RawRead::Response(r) => (r.to_response(), "HTTP_1_1"),
            RawRead::Closed => {
                return Err(anyhow!("the tunnel to {host} closed without answering").into());
            }
        }
    };
    w.tls = Some(TlsExchange {
        chain: conn.peer_certificates().to_vec(),
        version,
    });
    w.last = Some(response);
    Ok(())
}

#[when(
    regex = r#"^an HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept proxy$"#
)]
async fn https_get_via_intercept(w: &mut PlaneA, host: String, path: String) -> Step {
    https_via_intercept(w, &unescape(&host), &unescape(&path)).await
}

#[when(
    regex = r#"^an HTTP/2 HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept proxy$"#
)]
async fn h2_get_via_intercept(w: &mut PlaneA, host: String, path: String) -> Step {
    https_via_intercept(w, &unescape(&host), &unescape(&path)).await
}

#[then(regex = r"^the response was served over HTTP/2$")]
async fn served_over_h2(w: &mut PlaneA) -> Step {
    let got = w
        .tls
        .as_ref()
        .ok_or_else(|| anyhow!("no HTTP/2 exchange was made in this scenario"))?
        .version;
    w.ensure(got == "HTTP_2", || {
        format!("expected ALPN to negotiate HTTP/2 through the tunnel, got {got}")
    })
}

/// The shared listener's rules, from `GET /intercept/rules` on `admin`, which must answer 200: a
/// failure never reads as "no rules".
async fn listed_rules(w: &mut PlaneA, admin: &str) -> anyhow::Result<Vec<Value>> {
    let r = intercept_admin(w, admin, "GET", "/intercept/rules", None).await?;
    if r.status != 200 {
        let head: String = r.body.chars().take(80).collect();
        bail!("GET /intercept/rules -> {}: {head}", r.status);
    }
    match serde_json::from_str::<Value>(&r.body)
        .with_context(|| format!("GET /intercept/rules is not JSON: {}", r.body))?
    {
        Value::Array(rules) => Ok(rules),
        other => bail!("GET /intercept/rules is not an array: {other}"),
    }
}

#[then(regex = r"^the intercept rules count is (\d+)$")]
async fn intercept_rules_count(w: &mut PlaneA, n: usize) -> Step {
    let admin = harness().admin_url.clone();
    let got = listed_rules(w, &admin).await?.len();
    w.ensure(got == n, || {
        format!("expected {n} intercept rule(s), got {got}")
    })
}

/// `DELETE /intercept/rules`: 404 is nothing to clear; any other refusal is a real admin fault.
async fn clear_rules(admin: &str) -> anyhow::Result<()> {
    let url = format!("{admin}/intercept/rules");
    let resp = clients()
        .verifying
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status().as_u16();
    if status >= 300 && status != 404 {
        bail!("DELETE {url} -> {status}");
    }
    Ok(())
}

#[when(regex = r"^the intercept rules are cleared$")]
async fn intercept_rules_cleared(w: &mut PlaneA) -> Step {
    let admin = harness().admin_url.clone();
    w.record_curl("DELETE", &format!("{admin}/intercept/rules"), &[], None);
    clear_rules(&admin).await?;
    Ok(())
}

// ── @intercept-runtime: the listener started and stopped over the admin API (rift#493) ──

async fn start_runtime_engine(w: &mut PlaneA, allow_injection: bool) -> Step {
    if let Some(old) = w.runtime_engine.take() {
        old.stop().await?;
    }
    w.runtime_engine = Some(harness().start_plain_engine(allow_injection).await?);
    Ok(())
}

fn runtime_admin(w: &PlaneA) -> anyhow::Result<String> {
    w.runtime_engine
        .as_ref()
        .map(|e| e.admin_url.clone())
        .ok_or_else(|| anyhow!("no runtime-intercept engine was started in this scenario"))
}

/// `method path` on the runtime engine, kept as the last response. A POST carries `body`.
async fn runtime_send(w: &mut PlaneA, method: &str, path: &str, body: &str) -> Step {
    let admin = runtime_admin(w)?;
    let body = (method == "POST").then_some(body);
    let r = intercept_admin(w, &admin, method, path, body).await?;
    w.last = Some(r);
    Ok(())
}

/// `POST /intercept` with `body` on the runtime engine, kept as the last response; a 201's listener
/// port (and returned CA) is kept for the tunnel steps.
async fn start_runtime_listener(w: &mut PlaneA, body: &str) -> Step {
    runtime_send(w, "POST", "/intercept", body).await?;
    let r = w.last()?;
    if r.status == 201 {
        let json: Value = serde_json::from_str(&r.body)
            .with_context(|| format!("POST /intercept 201 is not JSON: {}", r.body))?;
        let port = json
            .get("interceptPort")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| anyhow!("POST /intercept 201 has no interceptPort: {}", r.body))?;
        let returned_ca = json
            .get("caCertPem")
            .and_then(Value::as_str)
            .map(str::to_string);
        w.runtime_listener = Some(RuntimeListener { port, returned_ca });
    }
    Ok(())
}

#[given(regex = r"^a rift server started without an intercept listener$")]
async fn runtime_engine(w: &mut PlaneA) -> Step {
    start_runtime_engine(w, true).await
}

#[given(regex = r"^a rift server started without an intercept listener or --allow-injection$")]
async fn runtime_engine_without_injection(w: &mut PlaneA) -> Step {
    start_runtime_engine(w, false).await
}

#[when(regex = r"^the intercept listener is queried over the admin API$")]
async fn runtime_query(w: &mut PlaneA) -> Step {
    runtime_send(w, "GET", "/intercept", "").await
}

#[when(regex = r"^the intercept listener is started over the admin API$")]
async fn runtime_start(w: &mut PlaneA) -> Step {
    runtime_send(w, "POST", "/intercept", "{}").await
}

#[when(regex = r"^the intercept listener is stopped over the admin API$")]
async fn runtime_stop(w: &mut PlaneA) -> Step {
    runtime_send(w, "DELETE", "/intercept", "").await
}

#[when(
    regex = r#"^the intercept listener is started over the admin API seeding a serve rule "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)"$"#
)]
async fn runtime_start_seeded(w: &mut PlaneA, body: String, host: String) -> Step {
    let options = serde_json::json!({"rules": [serve_rule(&unescape(&host), &unescape(&body))]});
    start_runtime_listener(w, &options.to_string()).await
}

#[when(regex = r"^the intercept listener is started over the admin API with returnCaKey$")]
async fn runtime_start_return_ca_key(w: &mut PlaneA) -> Step {
    start_runtime_listener(w, r#"{"returnCaKey":true}"#).await
}

fn test_ca_key_pem() -> anyhow::Result<String> {
    let path = fixtures_dir().join("intercept").join("ca-key.pem");
    std::fs::read_to_string(&path).with_context(|| format!("the test CA key {}", path.display()))
}

#[when(
    regex = r"^the intercept listener is started over the admin API with the test CA as inline PEM$"
)]
async fn runtime_start_inline_ca(w: &mut PlaneA) -> Step {
    let options = serde_json::json!({"caCertPem": test_ca_pem()?, "caKeyPem": test_ca_key_pem()?});
    start_runtime_listener(w, &options.to_string()).await
}

#[when(
    regex = r#"^the intercept listener is started over the admin API with the "((?:\\.|[^"\\])*)" options$"#
)]
async fn runtime_start_with_options(w: &mut PlaneA, options: String) -> Step {
    let body = match unescape(&options).as_str() {
        "returnCaKey with a supplied CA" => serde_json::json!({
            "returnCaKey": true, "caCertPem": test_ca_pem()?, "caKeyPem": test_ca_key_pem()?
        })
        .to_string(),
        "half PEM pair" => serde_json::json!({"caCertPem": test_ca_pem()?}).to_string(),
        "DNS host" => r#"{"host":"localhost"}"#.to_string(),
        "unknown field" => r#"{"bogus":1}"#.to_string(),
        other => return Err(anyhow!("unknown start-options case '{other}'").into()),
    };
    start_runtime_listener(w, &body).await
}

#[then(regex = r"^the runtime intercept rules count is (\d+)$")]
async fn runtime_rules_count(w: &mut PlaneA, n: usize) -> Step {
    let admin = runtime_admin(w)?;
    let got = listed_rules(w, &admin).await?.len();
    w.ensure(got == n, || {
        format!("expected {n} runtime intercept rule(s), got {got}")
    })
}

#[then(regex = r"^the runtime intercept CA PEM is the test CA$")]
async fn runtime_ca_is_test_ca(w: &mut PlaneA) -> Step {
    let admin = runtime_admin(w)?;
    let expected = test_ca_pem()?;
    let r = ca_pem_of(w, &admin).await?;
    w.ensure(r.status == 200 && r.body.trim() == expected.trim(), || {
        let head: String = r.body.chars().take(80).collect();
        format!(
            "GET /intercept/ca.pem ({}) should equal the inline test CA, got '{head}'",
            r.status
        )
    })
}

/// A tunnel through the runtime listener trusting `ca_pem`, then `GET path` on it.
async fn runtime_listener_get(w: &mut PlaneA, host: &str, path: &str, ca_pem: &str) -> Step {
    let port = w
        .runtime_listener
        .as_ref()
        .map(|l| l.port)
        .ok_or_else(|| anyhow!("no runtime listener was started in this scenario"))?;
    let roots = tls::certs_from_pem(ca_pem.as_bytes())?;
    w.curl.push(format!(
        "curl -sv --http1.1 --proxy http://127.0.0.1:{port} --cacert <the listener's CA> https://{host}{path}"
    ));
    let conn = intercept::tunnel(
        "127.0.0.1",
        port,
        host,
        443,
        &roots,
        tls::ALPN_HTTP1,
        &[],
        None,
    )
    .await?;
    raw_get_on(w, conn, host, path).await
}

#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the runtime intercept listener$"#
)]
async fn raw_get_runtime(w: &mut PlaneA, host: String, path: String) -> Step {
    let admin = runtime_admin(w)?;
    let ca = ca_pem_of(w, &admin).await?;
    if ca.status != 200 {
        return Err(anyhow!("GET /intercept/ca.pem -> {}: {}", ca.status, ca.body).into());
    }
    runtime_listener_get(w, &unescape(&host), &unescape(&path), &ca.body).await
}

#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the runtime intercept listener trusting the returned CA$"#
)]
async fn raw_get_runtime_returned_ca(w: &mut PlaneA, host: String, path: String) -> Step {
    let ca = w
        .runtime_listener
        .as_ref()
        .ok_or_else(|| anyhow!("no runtime listener was started in this scenario"))?
        .returned_ca
        .clone()
        .ok_or_else(|| anyhow!("the start response carried no caCertPem"))?;
    runtime_listener_get(w, &unescape(&host), &unescape(&path), &ca).await
}

#[when(
    regex = r"^a clean serve rule and a rule with an inject predicate nested under not are posted as one runtime intercept batch$"
)]
async fn runtime_poisoned_batch(w: &mut PlaneA) -> Step {
    let poisoned = serde_json::json!({
        "host": "poisoned.test",
        "predicates": [{"not": {"inject": "function (r) { return true; }"}}],
        "action": {"serve": {"statusCode": 200}}
    });
    let batch = Value::Array(vec![serve_rule("clean.test", "clean"), poisoned]);
    runtime_send(w, "POST", "/intercept/rules", &batch.to_string()).await
}

#[when(regex = r"^a batch of (\d+) serve rules is posted to the runtime intercept rules$")]
async fn runtime_rule_batch(w: &mut PlaneA, n: usize) -> Step {
    let batch: Vec<Value> = (0..n)
        .map(|i| serve_rule(&format!("cap-{i}.test"), "x"))
        .collect();
    let body = Value::Array(batch).to_string();
    // The repro would otherwise carry a 10,000-rule body.
    let admin = runtime_admin(w)?;
    let url = format!("{admin}/intercept/rules");
    w.curl.push(format!(
        "curl -X POST -H 'Content-Type: application/json' -d '<{n} serve rules>' '{url}'"
    ));
    let (resp, _) = exchange(
        &clients().verifying,
        "POST",
        &url,
        &json_content_type(),
        Some(&body),
    )
    .await?
    .with_context(|| format!("transport failure on POST {url}"))?;
    w.last = Some(resp);
    Ok(())
}

// ── @intercept-auth: the --intercept-auth listener (rift#878) ──

fn intercept_engine(w: &PlaneA) -> anyhow::Result<(String, u16)> {
    w.intercept_engine
        .as_ref()
        .map(|e| (e.admin_url.clone(), e.intercept_port))
        .ok_or_else(|| anyhow!("no intercept engine of its own was started in this scenario"))
}

async fn start_own_intercept_engine(
    w: &mut PlaneA,
    auth: Option<&str>,
    upstream_ca: Option<&std::path::Path>,
) -> Step {
    if let Some(old) = w.intercept_engine.take() {
        old.stop().await?;
    }
    w.intercept_engine = Some(harness().start_intercept_engine(auth, upstream_ca).await?);
    Ok(())
}

#[given(regex = r"^a rift server whose intercept listener requires proxy auth$")]
async fn auth_engine(w: &mut PlaneA) -> Step {
    start_own_intercept_engine(w, Some(INTERCEPT_AUTH), None).await
}

/// A rule on the scenario's own intercept engine, requiring a 201.
async fn add_own_engine_rule(w: &mut PlaneA, body: String, host: String) -> Step {
    let (admin, _) = intercept_engine(w)?;
    let rule = serve_rule(&unescape(&host), &unescape(&body)).to_string();
    let r = intercept_admin(w, &admin, "POST", "/intercept/rules", Some(&rule)).await?;
    w.ensure(r.status == 201, || {
        format!("POST /intercept/rules -> {}: {}", r.status, r.body)
    })
}

#[given(
    regex = r#"^an intercept rule on the authenticated engine serves "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)"$"#
)]
async fn auth_engine_rule(w: &mut PlaneA, body: String, host: String) -> Step {
    add_own_engine_rule(w, body, host).await
}

/// A bare `CONNECT host:443` on a plaintext connection to the auth listener; the answer is kept,
/// and the connection too, to observe the close.
async fn connect_to_auth_listener(w: &mut PlaneA, host: &str, auth: Option<String>) -> Step {
    let (_, port) = intercept_engine(w)?;
    let headers: Vec<(&str, &str)> = auth
        .as_deref()
        .map(|a| vec![("Proxy-Authorization", a)])
        .unwrap_or_default();
    w.curl.push(format!(
        "printf 'CONNECT {host}:443 HTTP/1.1\\r\\nHost: {host}:443\\r\\n\\r\\n' | nc 127.0.0.1 {port}"
    ));
    let mut conn = RawConnection::plain("127.0.0.1", port).await?;
    let read = conn
        .exchange(&wire::connect_request(host, 443, &headers))
        .await?;
    w.listener_conn = Some(conn);
    w.keep_raw(read, host)
}

#[when(
    regex = r#"^a CONNECT to "((?:\\.|[^"\\])*)" is sent to the authenticated intercept listener without credentials$"#
)]
async fn connect_without_credentials(w: &mut PlaneA, host: String) -> Step {
    connect_to_auth_listener(w, &unescape(&host), None).await
}

#[when(
    regex = r#"^a CONNECT to "((?:\\.|[^"\\])*)" is sent to the authenticated intercept listener with credentials "((?:\\.|[^"\\])*)"$"#
)]
async fn connect_with_credentials(w: &mut PlaneA, host: String, credentials: String) -> Step {
    let auth = intercept::basic_auth(&unescape(&credentials));
    connect_to_auth_listener(w, &unescape(&host), Some(auth)).await
}

#[then(regex = r"^the response carries a Basic proxy-authenticate challenge$")]
async fn basic_challenge(w: &mut PlaneA) -> Step {
    let challenge: Vec<String> = w
        .raw_response()?
        .header("proxy-authenticate")
        .into_iter()
        .map(str::to_string)
        .collect();
    w.ensure(challenge.iter().any(|c| c.starts_with("Basic")), || {
        format!("expected a `Proxy-Authenticate: Basic …` challenge, got {challenge:?}")
    })
}

/// The listener closes the plaintext connection after a refusal instead of serving another
/// request on it.
async fn listener_closes(w: &mut PlaneA, after: &str) -> Step {
    let read = w
        .listener_conn
        .as_mut()
        .ok_or_else(|| anyhow!("no listener connection was opened in this scenario"))?
        .read_next()
        .await?;
    w.ensure(read == RawRead::Closed, || {
        format!("expected the listener to close after the {after}, but it sent another response: {read:?}")
    })
}

#[then(regex = r"^the authenticated intercept listener then closes the connection$")]
async fn auth_listener_closes(w: &mut PlaneA) -> Step {
    listener_closes(w, "407").await
}

#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the authenticated intercept listener with credentials "((?:\\.|[^"\\])*)"$"#
)]
async fn raw_get_authenticated(
    w: &mut PlaneA,
    host: String,
    path: String,
    credentials: String,
) -> Step {
    let (host, path) = (unescape(&host), unescape(&path));
    let (_, port) = intercept_engine(w)?;
    let auth = intercept::basic_auth(&unescape(&credentials));
    w.curl.push(format!(
        "curl -sv --http1.1 --proxy http://{}@127.0.0.1:{port} --cacert {} https://{host}{path}",
        unescape(&credentials),
        test_ca_path().display()
    ));
    let conn = intercept::tunnel(
        "127.0.0.1",
        port,
        &host,
        443,
        &test_ca_roots()?,
        tls::ALPN_HTTP1,
        &[("Proxy-Authorization", &auth)],
        None,
    )
    .await?;
    raw_get_on(w, conn, &host, &path).await
}

// ── @intercept-raw: framing and connection handling inside the tunnel (rift#991, rift#993) ──

#[when(
    regex = r#"^a raw HTTPS POST of (\d+) body bytes to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_post_bytes(w: &mut PlaneA, n: usize, host: String, path: String) -> Step {
    let host = unescape(&host);
    let request = wire::request("POST", &host, &unescape(&path), &[], &vec![b'a'; n]);
    raw_exchange(w, &host, &request).await
}

#[then(regex = r"^the current imposter has recorded (\d+) requests$")]
async fn current_imposter_recorded(w: &mut PlaneA, n: i64) -> Step {
    let got = number_of_requests(w, w.current()?).await?;
    w.ensure(got == n, || {
        format!("expected the imposter to have recorded {n} request(s), got {got}")
    })
}

#[when(
    regex = r#"^a raw HTTPS POST of the chunked body "((?:\\.|[^"\\])*)" to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_post_chunked(w: &mut PlaneA, body: String, host: String, path: String) -> Step {
    let (body, host) = (unescape(&body), unescape(&host));
    // Several chunks, so the body only reads right if the engine reassembles them.
    let chars: Vec<char> = body.chars().collect();
    let chunks: Vec<String> = chars.chunks(5).map(|c| c.iter().collect()).collect();
    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
    let request = wire::chunked_request("POST", &host, &unescape(&path), &chunks);
    raw_exchange(w, &host, &request).await
}

#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_get_tunnel(w: &mut PlaneA, host: String, path: String) -> Step {
    let host = unescape(&host);
    let request = wire::request("GET", &host, &unescape(&path), &[], b"");
    raw_exchange(w, &host, &request).await
}

// Two separate header lines of one name, which a client library may fold into one.
#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" with header "((?:\\.|[^"\\])*)" sent as "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_get_repeated_header(
    w: &mut PlaneA,
    host: String,
    path: String,
    name: String,
    first: String,
    second: String,
) -> Step {
    let (host, name, first, second) = (
        unescape(&host),
        unescape(&name),
        unescape(&first),
        unescape(&second),
    );
    let headers = [
        (name.as_str(), first.as_str()),
        (name.as_str(), second.as_str()),
    ];
    let request = wire::request("GET", &host, &unescape(&path), &headers, b"");
    raw_exchange(w, &host, &request).await
}

#[then(regex = r"^the intercept tunnel answered (\d+) requests on one connection$")]
async fn tunnel_answered(w: &mut PlaneA, n: usize) -> Step {
    let (host, got) = w
        .tunnel
        .as_ref()
        .map(|(h, c)| (h.clone(), c.responses_read()))
        .ok_or_else(|| anyhow!("no raw tunnel was opened in this scenario"))?;
    w.ensure(got == n, || {
        format!("expected {n} responses on the one tunnel to {host}, got {got}")
    })
}

// The body is back-to-back copies of a complete `GET <smuggled>` request: if the engine ever framed
// the unread tail of the refused body as requests, the next response would be the smuggled one.
#[when(
    regex = r#"^a raw HTTPS POST of (\d+) body bytes of pipelined "((?:\\.|[^"\\])*)" requests to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_post_pipelined(
    w: &mut PlaneA,
    n: usize,
    smuggled: String,
    host: String,
    path: String,
) -> Step {
    let host = unescape(&host);
    let unit = wire::request("GET", &host, &unescape(&smuggled), &[], b"");
    let body: Vec<u8> = unit.iter().copied().cycle().take(n).collect();
    let request = wire::request("POST", &host, &unescape(&path), &[], &body);
    raw_exchange(w, &host, &request).await
}

// rift#993 allows either outcome after a 413: the connection is reused (and the tail discarded),
// or it is closed. What it must never do is answer from the leftover bytes.
#[then(
    regex = r#"^the next raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" on the same tunnel is answered "((?:\\.|[^"\\])*)" or the connection is closed$"#
)]
async fn next_get_answered_or_closed(
    w: &mut PlaneA,
    host: String,
    path: String,
    expected: String,
) -> Step {
    let (host, path, expected) = (unescape(&host), unescape(&path), unescape(&expected));
    let request = wire::request("GET", &host, &path, &[], b"");
    let read = open_tunnel_to(w, &host)?.exchange(&request).await?;
    match read {
        RawRead::Closed => Ok(()),
        RawRead::Response(r) => {
            let body = r.body_text();
            w.ensure(r.status == 200 && body == expected, || {
                let head: String = body.chars().take(80).collect();
                format!(
                    "after the 413, GET {path} on the same tunnel should be answered 200 '{expected}' (or the connection closed), got {} '{head}'",
                    r.status
                )
            })
        }
    }
}

#[when(regex = r#"^a plain GET for "((?:\\.|[^"\\])*)" is sent to the intercept listener$"#)]
async fn plain_get_to_listener(w: &mut PlaneA, url: String) -> Step {
    let url = unescape(&url);
    let host = reqwest::Url::parse(&url)
        .with_context(|| format!("URL {url}"))?
        .host_str()
        .ok_or_else(|| anyhow!("{url} has no host"))?
        .to_string();
    let port = harness().intercept_port;
    let proxy = harness().imposter_host();
    w.curl
        .push(format!("curl -sv --proxy http://{proxy}:{port} '{url}'"));
    let mut conn = RawConnection::plain(proxy, port).await?;
    let read = conn
        .exchange(&wire::request("GET", &host, &url, &[], b""))
        .await?;
    w.listener_conn = Some(conn);
    match read {
        RawRead::Response(r) => {
            w.last = Some(r.to_response());
            Ok(())
        }
        RawRead::Closed => Err(anyhow!("the intercept listener closed without answering").into()),
    }
}

#[then(regex = r"^the intercept listener then closes the connection$")]
async fn shared_listener_closes(w: &mut PlaneA) -> Step {
    listener_closes(w, "refusal").await
}

/// A serve rule for `host``path` whose body is the one-field object `{key: value}`: as a JSON
/// object, or as that object already rendered to an escaped JSON string (the pre-rift#933 form).
fn object_body_rule(host: &str, path: &str, key: &str, value: &str, as_string: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert(key.to_string(), value.into());
    let obj = Value::Object(obj);
    let body = if as_string {
        Value::String(obj.to_string())
    } else {
        obj
    };
    serde_json::json!({
        "host": host,
        "predicates": [{"equals": {"path": path}}],
        "action": {"serve": {"statusCode": 200, "body": body}}
    })
}

#[given(
    regex = r#"^an intercept rule for host "((?:\\.|[^"\\])*)" path "((?:\\.|[^"\\])*)" serves the object body with "((?:\\.|[^"\\])*)" set to "((?:\\.|[^"\\])*)"$"#
)]
async fn rule_object_body(
    w: &mut PlaneA,
    host: String,
    path: String,
    key: String,
    value: String,
) -> Step {
    let rule = object_body_rule(
        &unescape(&host),
        &unescape(&path),
        &unescape(&key),
        &unescape(&value),
        false,
    );
    add_shared_rule(w, &rule).await
}

#[given(
    regex = r#"^an intercept rule for host "((?:\\.|[^"\\])*)" path "((?:\\.|[^"\\])*)" serves the escaped-string body with "((?:\\.|[^"\\])*)" set to "((?:\\.|[^"\\])*)"$"#
)]
async fn rule_escaped_string_body(
    w: &mut PlaneA,
    host: String,
    path: String,
    key: String,
    value: String,
) -> Step {
    let rule = object_body_rule(
        &unescape(&host),
        &unescape(&path),
        &unescape(&key),
        &unescape(&value),
        true,
    );
    add_shared_rule(w, &rule).await
}

#[then(
    regex = r#"^the response body is the compact JSON object with "((?:\\.|[^"\\])*)" set to "((?:\\.|[^"\\])*)"$"#
)]
async fn body_is_compact_object(w: &mut PlaneA, key: String, value: String) -> Step {
    let mut obj = serde_json::Map::new();
    obj.insert(unescape(&key), unescape(&value).into());
    let expected = Value::Object(obj).to_string();
    let got = w.raw_response()?.body_text();
    w.ensure(got == expected, || {
        format!("expected body '{expected}', got '{got}'")
    })
}

#[then(regex = r"^the response content-length matches its body$")]
async fn content_length_matches(w: &mut PlaneA) -> Step {
    let r = w.raw_response()?;
    let declared: Vec<String> = r
        .header("content-length")
        .into_iter()
        .map(str::to_string)
        .collect();
    let len = r.body.len();
    w.ensure(declared == [len.to_string()], || {
        format!("content-length {declared:?} does not frame the {len}-byte body")
    })
}

#[then(
    regex = r#"^the response body is byte-identical to a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn body_byte_identical(w: &mut PlaneA, host: String, path: String) -> Step {
    let first = w.raw_response()?.clone();
    let host = unescape(&host);
    let request = wire::request("GET", &host, &unescape(&path), &[], b"");
    raw_exchange(w, &host, &request).await?;
    let second = w.raw_response()?;
    w.ensure(
        first.body == second.body && first.status == second.status,
        || {
            format!(
                "expected identical bodies, got {} '{}' vs {} '{}'",
                first.status,
                first.body_text(),
                second.status,
                second.body_text()
            )
        },
    )
}

/// The `action.serve` object of the listed rule for `host`.
async fn listed_serve(w: &mut PlaneA, host: &str) -> anyhow::Result<Option<Value>> {
    let admin = harness().admin_url.clone();
    Ok(listed_rules(w, &admin)
        .await?
        .into_iter()
        .find(|r| r.get("host").and_then(Value::as_str) == Some(host))
        .and_then(|r| r.get("action")?.get("serve").cloned()))
}

#[then(
    regex = r#"^the intercept rule for host "((?:\\.|[^"\\])*)" lists its serve body as a JSON object with "((?:\\.|[^"\\])*)" set to "((?:\\.|[^"\\])*)"$"#
)]
async fn listed_object_body(w: &mut PlaneA, host: String, key: String, value: String) -> Step {
    let (host, key, value) = (unescape(&host), unescape(&key), unescape(&value));
    let body = listed_serve(w, &host)
        .await?
        .and_then(|s| s.get("body").cloned());
    let mut obj = serde_json::Map::new();
    obj.insert(key.clone(), value.clone().into());
    let want = Value::Object(obj);
    w.ensure(body.as_ref() == Some(&want), || {
        format!(
            "expected GET /intercept/rules to list the {host} serve body as the object {{{key}: {value}}}, got {body:?}"
        )
    })
}

// The fall-through body is one text line: the message plus a trailing newline.
#[then(regex = r#"^the response body is the line "((?:\\.|[^"\\])*)"$"#)]
async fn body_is_line(w: &mut PlaneA, line: String) -> Step {
    let line = unescape(&line);
    let got = w.raw_response()?.body_text();
    w.ensure(got == format!("{line}\n"), || {
        format!("expected body '{line}\\n', got '{got}'")
    })
}

#[given(
    regex = r#"^an intercept rule for host "((?:\\.|[^"\\])*)" serves status "((?:\\.|[^"\\])*)" with "((?:\\.|[^"\\])*)" as "((?:\\.|[^"\\])*)" and "((?:\\.|[^"\\])*)" and "((?:\\.|[^"\\])*)" as the number (\d+)$"#
)]
#[allow(clippy::too_many_arguments)]
async fn rule_widened_serve(
    w: &mut PlaneA,
    host: String,
    status: String,
    multi: String,
    a: String,
    b: String,
    scalar: String,
    n: i64,
) -> Step {
    let mut headers = serde_json::Map::new();
    headers.insert(
        unescape(&multi),
        Value::Array(vec![unescape(&a).into(), unescape(&b).into()]),
    );
    headers.insert(unescape(&scalar), n.into());
    let rule = serde_json::json!({
        "host": unescape(&host),
        "action": {"serve": {"statusCode": unescape(&status), "headers": headers, "body": "widened"}}
    });
    add_shared_rule(w, &rule).await
}

#[then(
    regex = r#"^the raw response carries "((?:\\.|[^"\\])*)" lines "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)"$"#
)]
async fn raw_header_lines(w: &mut PlaneA, name: String, first: String, second: String) -> Step {
    let (name, first, second) = (unescape(&name), unescape(&first), unescape(&second));
    let lines: Vec<String> = w
        .raw_response()?
        .header(&name)
        .into_iter()
        .map(str::to_string)
        .collect();
    w.ensure(lines == [first.clone(), second.clone()], || {
        format!("expected two {name} lines [{first}, {second}] in order, got {lines:?}")
    })
}

#[then(
    regex = r#"^the intercept rule for host "((?:\\.|[^"\\])*)" lists statusCode (\d+) as a number and header "((?:\\.|[^"\\])*)" as the string "((?:\\.|[^"\\])*)"$"#
)]
async fn listed_widened(
    w: &mut PlaneA,
    host: String,
    status: i64,
    header: String,
    value: String,
) -> Step {
    let (host, header, value) = (unescape(&host), unescape(&header), unescape(&value));
    let serve = listed_serve(w, &host).await?;
    let listed_status = serve.as_ref().and_then(|s| s.get("statusCode")).cloned();
    let listed_header = serve
        .as_ref()
        .and_then(|s| s.get("headers")?.get(&header))
        .cloned();
    w.ensure(
        listed_status.as_ref().is_some_and(|s| is_number(s, status))
            && listed_header.as_ref().and_then(Value::as_str) == Some(value.as_str()),
        || {
            format!(
                "expected {host} listed with statusCode {status} (number) and {header} \"{value}\" (string), got statusCode={listed_status:?} {header}={listed_header:?}"
            )
        },
    )
}

#[given(regex = r#"^an intercept rule for host "((?:\\.|[^"\\])*)" serves 200 with no body$"#)]
async fn rule_no_body(w: &mut PlaneA, host: String) -> Step {
    let rule =
        serde_json::json!({"host": unescape(&host), "action": {"serve": {"statusCode": 200}}});
    add_shared_rule(w, &rule).await
}

#[then(regex = r"^the raw response has content-length 0 and an empty body$")]
async fn raw_empty_body(w: &mut PlaneA) -> Step {
    let r = w.raw_response()?;
    let declared: Vec<String> = r
        .header("content-length")
        .into_iter()
        .map(str::to_string)
        .collect();
    let len = r.body.len();
    w.ensure(declared == ["0"] && len == 0, || {
        format!("expected content-length [0] and an empty body, got {declared:?} and {len} bytes")
    })
}

#[when(
    regex = r#"^a raw HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" with SNI and Host "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_get_sni(w: &mut PlaneA, connect_host: String, path: String, name: String) -> Step {
    let (connect_host, path, name) = (unescape(&connect_host), unescape(&path), unescape(&name));
    let port = harness().intercept_port;
    let proxy = harness().imposter_host();
    w.curl.push(format!(
        "# CONNECT {connect_host}:443 via {proxy}:{port}, then TLS with SNI {name} and Host: {name}"
    ));
    let roots = shared_truststore_roots().await?;
    let mut conn = intercept::tunnel(
        proxy,
        port,
        &connect_host,
        443,
        &roots,
        tls::ALPN_HTTP1,
        &[],
        Some(&name),
    )
    .await?;
    let read = conn
        .exchange(&wire::request("GET", &name, &path, &[], b""))
        .await?;
    w.keep_raw(read, &connect_host)
}

#[when(
    regex = r#"^a raw HTTPS POST with the chunk-size line "((?:\\.|[^"\\])*)" to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the intercept tunnel$"#
)]
async fn raw_post_bad_chunk(w: &mut PlaneA, size_line: String, host: String, path: String) -> Step {
    let host = unescape(&host);
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\r\n{}\r\nhello\r\n0\r\n\r\n",
        unescape(&path),
        unescape(&size_line)
    );
    raw_exchange(w, &host, request.as_bytes()).await
}

#[given(regex = r#"^an intercept rule forwards host "((?:\\.|[^"\\])*)" to a closed port$"#)]
async fn rule_forwards_to_closed_port(w: &mut PlaneA, host: String) -> Step {
    // A port nothing listens on, the use `free_port` is kept for.
    let port = free_port()?;
    let rule = serde_json::json!({"host": unescape(&host), "action": {"forward": {"port": port}}});
    add_shared_rule(w, &rule).await
}

#[then(
    regex = r#"^the intercept JKS truststore with password "((?:\\.|[^"\\])*)" echoes the password and holds the intercept CA$"#
)]
async fn jks_truststore(w: &mut PlaneA, password: String) -> Step {
    let password = unescape(&password);
    let admin = harness().admin_url.clone();
    let url = format!("{admin}/intercept/truststore.jks?password={password}");
    w.record_curl("GET", &url, &[], None);
    let (status, headers, jks) = get_bytes(&url).await?;
    let ca = ca_pem_of(w, &admin).await?;
    let ca_der = tls::certs_from_pem(ca.body.as_bytes())?;
    let echoed = header_str(&headers, "x-truststore-password");
    let magic: String = jks.iter().take(4).map(|b| format!("{b:02X}")).collect();
    let held = intercept::jks_trusted_certificates(&jks, &password);
    let holds_ca = held.as_ref().is_ok_and(|certs| {
        certs
            .iter()
            .any(|c| ca_der.iter().any(|d| d.as_ref() == c.as_slice()))
    });
    w.ensure(
        status == 200 && echoed == password && holds_ca && magic == "FEEDFEED",
        || {
            format!(
                "truststore.jks -> {status}, x-truststore-password='{echoed}', holds CA={holds_ca} ({}), magic={magic} (JKS is FEEDFEED)",
                held.as_ref().err().map_or_else(|| "parsed".to_string(), |e| format!("{e:#}"))
            )
        },
    )
}

#[then(regex = r#"^the intercept CA PEM is served as "((?:\\.|[^"\\])*)"$"#)]
async fn ca_pem_content_type(w: &mut PlaneA, content_type: String) -> Step {
    let content_type = unescape(&content_type);
    let admin = harness().admin_url.clone();
    let r = ca_pem_of(w, &admin).await?;
    let got = r.header_values("content-type").to_vec();
    w.ensure(r.status == 200 && got == [content_type.clone()], || {
        format!(
            "GET /intercept/ca.pem -> {}, content-type {got:?}",
            r.status
        )
    })
}

// ── @intercept-ws: WebSocket passthrough (rift#997) ──

#[given(regex = r"^a TLS WebSocket echo origin on localhost$")]
async fn ws_echo_origin(w: &mut PlaneA) -> Step {
    let cert =
        std::fs::read(fixtures_dir().join("tls/server-cert.pem")).context("server-cert.pem")?;
    let key = std::fs::read(fixtures_dir().join("tls/server-key.pem")).context("server-key.pem")?;
    w.ws_origin = Some(intercept::WsEchoOrigin::start(&cert, &key).await?);
    Ok(())
}

#[given(regex = r"^an intercept engine that trusts the test CA upstream$")]
async fn ws_intercept_engine(w: &mut PlaneA) -> Step {
    start_own_intercept_engine(w, None, Some(&test_ca_path())).await
}

#[given(
    regex = r#"^an intercept rule on that engine serves "((?:\\.|[^"\\])*)" for host "((?:\\.|[^"\\])*)"$"#
)]
async fn ws_engine_rule(w: &mut PlaneA, body: String, host: String) -> Step {
    add_own_engine_rule(w, body, host).await
}

// ── @intercept-upstream-trust: a forward to an `https` target (rift#1273) ──

#[given(
    regex = r#"^an intercept rule on that engine forwards host "((?:\\.|[^"\\])*)" to the HTTPS upstream at host "((?:\\.|[^"\\])*)" with scheme "((?:\\.|[^"\\])*)"$"#
)]
async fn engine_rule_forwards_to_https_upstream(
    w: &mut PlaneA,
    host: String,
    target: String,
    scheme: String,
) -> Step {
    let (admin, _) = intercept_engine(w)?;
    let port = w
        .upstream_port
        .ok_or_else(|| anyhow!("no HTTPS upstream imposter in this scenario"))?;
    let rule = serde_json::json!({
        "host": unescape(&host),
        "action": {"forward": {"port": port, "host": unescape(&target), "scheme": unescape(&scheme)}}
    })
    .to_string();
    let r = intercept_admin(w, &admin, "POST", "/intercept/rules", Some(&rule)).await?;
    w.ensure(r.status == 201, || {
        format!("POST /intercept/rules -> {}: {}", r.status, r.body)
    })
}

/// A GET through the scenario's own intercept listener, trusting the CA that engine exports: it is
/// minted per engine in some modes, so the committed test CA is not assumed.
#[when(
    regex = r#"^an HTTPS GET to "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through that engine's intercept listener$"#
)]
async fn https_get_via_own_engine(w: &mut PlaneA, host: String, path: String) -> Step {
    let (host, path) = (unescape(&host), unescape(&path));
    let (admin, port) = intercept_engine(w)?;
    let ca = ca_pem_of(w, &admin).await?;
    w.ensure(ca.status == 200, || {
        format!("GET /intercept/ca.pem -> {}: {}", ca.status, ca.body)
    })?;
    let roots = tls::certs_from_pem(ca.body.as_bytes())?;
    w.curl.push(format!(
        "curl -sv --proxy http://127.0.0.1:{port} --cacert <engine CA> https://{host}{path}"
    ));
    let mut conn = intercept::tunnel(
        "127.0.0.1",
        port,
        &host,
        443,
        &roots,
        tls::ALPN_HTTP1,
        &[],
        None,
    )
    .await?;
    let read = conn
        .exchange(&wire::request("GET", &host, &path, &[], b""))
        .await?;
    w.keep_raw(read, &host)
}

fn ws_origin(w: &PlaneA) -> anyhow::Result<&intercept::WsEchoOrigin> {
    w.ws_origin
        .as_ref()
        .ok_or_else(|| anyhow!("no WebSocket echo origin in this scenario"))
}

#[when(
    regex = r"^a WebSocket upgrade to the echo origin is sent through that engine's intercept listener$"
)]
async fn ws_upgrade(w: &mut PlaneA) -> Step {
    let (_, port) = intercept_engine(w)?;
    let origin = ws_origin(w)?.port;
    w.curl.push(format!(
        "# CONNECT localhost:{origin} via 127.0.0.1:{port}, TLS trusting the test CA, then GET /echo with Upgrade: websocket"
    ));
    let mut conn = intercept::tunnel(
        "127.0.0.1",
        port,
        "localhost",
        origin,
        &test_ca_roots()?,
        tls::ALPN_HTTP1,
        &[],
        None,
    )
    .await?;
    let request = wire::request(
        "GET",
        &format!("localhost:{origin}"),
        "/echo",
        &[
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
        ],
        b"",
    );
    let read = conn.upgrade(&request).await?;
    w.tunnel = Some(("localhost".to_string(), conn));
    match read {
        RawRead::Response(r) => {
            w.last = Some(r.to_response());
            w.raw_response = Some(r);
            Ok(())
        }
        RawRead::Closed => Err(anyhow!(
            "the intercept tunnel closed without answering the upgrade (origin error: {})",
            ws_origin(w)?.last_error()
        )
        .into()),
    }
}

// A blind TCP tunnel would reach the origin too, and the client trusts the CA that signed the
// origin's leaf. Only a leaf that is not the origin's shows the engine terminated TLS and relayed.
#[then(
    regex = r"^the tunnel's TLS certificate was minted by the intercept listener, not the origin's$"
)]
async fn ws_leaf_minted(w: &mut PlaneA) -> Step {
    let origin = tls::certs_from_pem(
        &std::fs::read(fixtures_dir().join("tls/server-cert.pem")).context("server-cert.pem")?,
    )?;
    let peer = open_tunnel_to(w, "localhost")?
        .peer_certificates()
        .first()
        .cloned();
    w.ensure(
        peer.as_ref().is_some_and(|p| origin.first() != Some(p)),
        || match &peer {
            Some(p) => format!(
                "expected an intercept-minted leaf on the tunnel, got the origin's own ({})",
                tls::subject(p).unwrap_or_else(|e| format!("{e:#}"))
            ),
            None => {
                "expected an intercept-minted leaf on the tunnel, got no certificate".to_string()
            }
        },
    )
}

// rift#1277: a minted leaf names the CA's key, so a verifier that looks the issuer up by key
// identifier (a JDK path builder, strict OpenSSL) finds it.
#[then(
    regex = r"^the tunnel's leaf certificate names the intercept CA's key as its Authority Key Identifier$"
)]
async fn leaf_names_ca_key(w: &mut PlaneA) -> Step {
    use x509_parser::extensions::ParsedExtension;
    let host = w
        .tunnel
        .as_ref()
        .map(|(h, _)| h.clone())
        .ok_or_else(|| anyhow!("no open raw tunnel in this scenario"))?;
    let leaf = open_tunnel_to(w, &host)?
        .peer_certificates()
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("the tunnel presented no certificate"))?;
    let admin = harness().admin_url.clone();
    let ca = ca_pem_of(w, &admin).await?;
    w.ensure(ca.status == 200, || {
        format!("GET /intercept/ca.pem -> {}: {}", ca.status, ca.body)
    })?;
    let ca_der = tls::certs_from_pem(ca.body.as_bytes())?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("/intercept/ca.pem holds no certificate"))?;
    let (_, ca_cert) = x509_parser::parse_x509_certificate(ca_der.as_ref())
        .map_err(|e| anyhow!("parsing the intercept CA: {e}"))?;
    let (_, leaf_cert) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| anyhow!("parsing the tunnel's leaf: {e}"))?;
    let ski: Option<Vec<u8>> =
        ca_cert
            .extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
                _ => None,
            });
    let aki: Option<Vec<u8>> =
        leaf_cert
            .extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(a) => {
                    a.key_identifier.as_ref().map(|k| k.0.to_vec())
                }
                _ => None,
            });
    let hex = |b: &Option<Vec<u8>>| {
        b.as_ref().map_or_else(
            || "<absent>".to_string(),
            |b| b.iter().map(|x| format!("{x:02x}")).collect::<String>(),
        )
    };
    w.ensure(aki.is_some() && aki == ski, || {
        format!(
            "expected the leaf's Authority Key Identifier to equal the CA's Subject Key Identifier, got AKI {} and SKI {}",
            hex(&aki),
            hex(&ski)
        )
    })
}

#[then(regex = r#"^a text frame "((?:\\.|[^"\\])*)" is echoed back as "((?:\\.|[^"\\])*)"$"#)]
async fn ws_echo(w: &mut PlaneA, sent: String, expected: String) -> Step {
    let (sent, expected) = (unescape(&sent), unescape(&expected));
    let frame = intercept::client_text_frame(&sent)?;
    let echoed = {
        let conn = open_tunnel_to(w, "localhost")?;
        match conn.write_raw(&frame).await {
            Ok(()) => intercept::read_frame(conn).await,
            Err(e) => Err(e),
        }
    };
    let (opcode, payload) = echoed.map_err(|e| {
        let why =
            ws_origin(w).map_or_else(|o| format!("{o:#}"), intercept::WsEchoOrigin::last_error);
        anyhow!("no echo frame ({e:#}; origin error: {why})")
    })?;
    let text = String::from_utf8_lossy(&payload).into_owned();
    w.ensure(opcode == 1 && text == expected, || {
        format!("expected a text frame '{expected}' back, got opcode {opcode} '{text}'")
    })
}

#[then(
    regex = r#"^the current imposter recorded header "((?:\\.|[^"\\])*)" as "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)"$"#
)]
async fn current_imposter_header_values(
    w: &mut PlaneA,
    name: String,
    first: String,
    second: String,
) -> Step {
    let (name, first, second) = (unescape(&name), unescape(&first), unescape(&second));
    let json = imposter_json(w, w.current()?).await?;
    // Every recorded request's values for `name` (case-insensitive): a single value records as a
    // string, a repeated header as an array.
    let values: Vec<String> = json
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("headers")?.as_object())
        .flat_map(|hs| hs.iter())
        .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
        .flat_map(|(_, v)| match v {
            Value::String(s) => vec![s.clone()],
            Value::Array(vs) => vs
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    w.ensure(values == [first.clone(), second.clone()], || {
        let head: String = json.to_string().chars().take(400).collect();
        format!(
            "expected the forwarded request to carry {name} as [{first}, {second}] in order, got {values:?}: {head}"
        )
    })
}

// ── front door and gateway dispatch (feature 16, #187) ───────────────────────────────────────

#[given(
    regex = r#"^the engine is started with the front door and routes fixture "((?:\\.|[^"\\])*)"$"#
)]
async fn start_with_front_door_fixture(w: &mut PlaneA, name: String) -> Step {
    let json = load_fixture(&unescape(&name))?;
    start_reload_engine(w, &json, true).await
}

/// `"x-t: a; x-t: b"` as `[(x-t, a), (x-t, b)]`, order kept.
fn header_list(spec: &str) -> anyhow::Result<Vec<(String, String)>> {
    spec.split(';')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| {
            h.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| anyhow!("header '{h}' is not 'name: value'"))
        })
        .collect()
}

#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the front door with host "((?:\\.|[^"\\])*)" and headers "((?:\\.|[^"\\])*)"$"#
)]
async fn send_through_front_door_with_headers(
    w: &mut PlaneA,
    method: String,
    path: String,
    host: String,
    headers: String,
) -> Step {
    let headers = header_list(&unescape(&headers))?;
    front_door_send(w, &method, &unescape(&path), &unescape(&host), headers).await
}

#[then(regex = r#"^the front door answered "((?:\\.|[^"\\])*)"$"#)]
async fn front_door_answered(w: &mut PlaneA, outcome: String) -> Step {
    let outcome = unescape(&outcome);
    let r = w.last()?;
    let marker = r.header_values("x-rift-front-door").to_vec();
    if outcome == "no-route" {
        w.ensure(r.status == 404 && marker == ["no-route"], || {
            format!(
                "expected 404 + x-rift-front-door: no-route, got {} marker={marker:?}: {}",
                r.status, r.body
            )
        })
    } else {
        w.ensure(
            r.status == 200 && r.body == outcome && marker.is_empty(),
            || {
                format!(
                    "expected 200 '{outcome}' from a routed imposter, got {} marker={marker:?}: {}",
                    r.status, r.body
                )
            },
        )
    }
}

/// The last entry of `GET {admin}/imposters/{port}`'s `requests`, on the scenario's engine.
async fn last_recorded_request(
    w: &PlaneA,
    port: u16,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let url = format!("{}/imposters/{port}", w.admin_url());
    let resp = get(&url).await?;
    if resp.status != 200 {
        bail!("GET {url} -> {} (expected 200): {}", resp.status, resp.body);
    }
    let json: Value = serde_json::from_str(&resp.body)
        .with_context(|| format!("invalid admin JSON (body={})", resp.body))?;
    match json
        .get("requests")
        .and_then(Value::as_array)
        .and_then(|r| r.last())
    {
        Some(Value::Object(fields)) => Ok(fields.clone()),
        _ => bail!("imposter {port} recorded no request: {}", resp.body),
    }
}

#[then(
    regex = r#"^imposter (\d+) last recorded path "((?:\\.|[^"\\])*)" with query "((?:\\.|[^"\\])*)" equal to "((?:\\.|[^"\\])*)"$"#
)]
async fn last_recorded_path_and_query(
    w: &mut PlaneA,
    port: u16,
    path: String,
    key: String,
    value: String,
) -> Step {
    let (path, key, value) = (unescape(&path), unescape(&key), unescape(&value));
    let req = last_recorded_request(w, port).await?;
    let got_path = req.get("path").and_then(Value::as_str);
    let got_value = req
        .get("query")
        .and_then(Value::as_object)
        .and_then(|q| q.get(&key))
        .and_then(Value::as_str);
    w.ensure(
        got_path == Some(path.as_str()) && got_value == Some(value.as_str()),
        || {
            format!(
                "expected imposter {port} to record path '{path}' with query {key}={value}, got path={got_path:?} query.{key}={got_value:?}"
            )
        },
    )
}

#[then(
    regex = r#"^imposter (\d+) last recorded header "((?:\\.|[^"\\])*)" equal to "((?:\\.|[^"\\])*)"$"#
)]
async fn last_recorded_header(w: &mut PlaneA, port: u16, header: String, value: String) -> Step {
    let (header, value) = (unescape(&header), unescape(&value));
    let req = last_recorded_request(w, port).await?;
    let got = req
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|hs| {
            hs.iter()
                .find(|(k, v)| k.eq_ignore_ascii_case(&header) && v.is_string())
        })
        .and_then(|(_, v)| v.as_str());
    w.ensure(got == Some(value.as_str()), || {
        format!("expected imposter {port} to record header {header}: {value}, got {got:?}")
    })
}

#[when(
    regex = r#"^the client sends a raw "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" through the front door with host "((?:\\.|[^"\\])*)"$"#
)]
async fn send_raw_through_front_door(
    w: &mut PlaneA,
    method: String,
    path: String,
    host: String,
) -> Step {
    let port = front_door_port(w)?;
    let addr = harness().imposter_host().to_string();
    capture_with_host(
        w,
        addr,
        port,
        unescape(&method),
        unescape(&path),
        unescape(&host),
        false,
    )
    .await
}

#[when(
    regex = r#"^an h2c client sends "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)" on one connection through the front door with host "((?:\\.|[^"\\])*)"$"#
)]
async fn h2c_through_front_door(
    w: &mut PlaneA,
    first: String,
    second: String,
    authority: String,
) -> Step {
    let port = front_door_port(w)?;
    let host = harness().imposter_host().to_string();
    let (first, second, authority) = (unescape(&first), unescape(&second), unescape(&authority));
    w.curl.push(format!(
        "curl -sv --http2-prior-knowledge -H 'Host: {authority}' http://{host}:{port}{first} http://{host}:{port}{second}"
    ));
    let streams = tokio::task::spawn_blocking(move || {
        raw::h2c_two_streams(&host, port, &authority, &first, &second)
    })
    .await
    .context("the h2c task")??;
    w.h2c = Some(streams);
    Ok(())
}

#[when(
    regex = r#"^an engine is booted with the front door and routes fixture "((?:\\.|[^"\\])*)"$"#
)]
async fn boot_front_door_engine(w: &mut PlaneA, name: String) -> Step {
    let name = unescape(&name);
    let json = load_fixture(&name)?;
    let config = harness().scratch_file("boot-config.json", &json)?;
    let flags = ["--configfile".to_string(), config.display().to_string()];
    w.curl.push(format!(
        "rift --front-door 0 --configfile {}   # fixture {name}; expected to refuse to boot",
        config.display()
    ));
    w.boot = Some(harness().boot_engine(&flags, true).await?);
    Ok(())
}

#[then(
    regex = r#"^the engine refused to boot with output naming "((?:\\.|[^"\\])*)" and "((?:\\.|[^"\\])*)"$"#
)]
async fn engine_refused_to_boot(w: &mut PlaneA, route: String, rule: String) -> Step {
    let (route, rule) = (unescape(&route), unescape(&rule));
    let boot = w
        .boot
        .as_ref()
        .ok_or_else(|| anyhow!("no engine boot attempted in this scenario"))?;
    let tail: Vec<&str> = boot.output.lines().collect();
    let tail = tail[tail.len().saturating_sub(5)..].join("\n");
    let refused = boot.exit_code.is_some_and(|c| c != 0)
        && boot.output.contains(&route)
        && boot.output.contains(&rule);
    let exit = boot.exit_code;
    w.ensure(refused, || {
        format!(
            "expected the boot to fail naming [{route}] and [{rule}], got exit={exit:?}:\n{tail}"
        )
    })
}

#[when(regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" to the admin port$"#)]
async fn send_to_admin_port(w: &mut PlaneA, method: String, path: String) -> Step {
    let url = format!("{}{}", harness().admin_url, unescape(&path));
    w.send_with(&clients().verifying, &method, &url, &[], None)
        .await
}

#[when(
    regex = r#"^the client sends "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)" via the gateway to a port with no imposter$"#
)]
async fn send_via_gateway_to_unknown_port(w: &mut PlaneA, method: String, path: String) -> Step {
    let admin = harness().admin_url.clone();
    let port = unknown_port(w, &admin).await?;
    let url = format!("{admin}/__rift/{port}{}", unescape(&path));
    w.send_with(&clients().verifying, &method, &url, &[], None)
        .await
}

// ── TLS termination and HTTP/2 negotiation (feature 20, #197) ────────────────────────────────

/// What a feature 20 HTTPS exchange settled: the chain the imposter presented and the HTTP version
/// ALPN chose, named as the JDK's `HttpClient.Version` is (`HTTP_2`, `HTTP_1_1`).
#[derive(Debug)]
struct TlsExchange {
    chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    version: &'static str,
}

/// The one stub every feature 20 imposter answers `/` with.
fn plain_tls_stub() -> Value {
    serde_json::json!({"responses": [{"is": {"statusCode": 200, "body": "tls-ok"}}]})
}

/// A stub on `path` only, so the plain stub still answers `/`.
fn stub_on_path(path: &str, response: Value) -> Value {
    serde_json::json!({"predicates": [{"equals": {"path": path}}], "responses": [response]})
}

fn tls_stubs(kind: &str) -> anyhow::Result<Vec<Value>> {
    Ok(match kind {
        "a plain stub" => vec![plain_tls_stub()],
        "a _rift.fault.tcp stub" => vec![
            stub_on_path(
                "/fault",
                serde_json::json!({"is": {"statusCode": 200}, "_rift": {"fault": {"tcp": "reset"}}}),
            ),
            plain_tls_stub(),
        ],
        "a _rift.script stub" => vec![
            stub_on_path(
                "/script",
                serde_json::json!({"_rift": {"script": {"engine": "rhai", "code": "fn respond(ctx) { http(200, #{}) }"}}}),
            ),
            plain_tls_stub(),
        ],
        other => bail!("unknown stub kind '{other}'"),
    })
}

/// An imposter of `protocol` with `fields` and `stubs`, without a port: the engine assigns one.
fn tls_imposter(
    protocol: &str,
    fields: Vec<(&str, Value)>,
    stubs: Vec<Value>,
) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("protocol".to_string(), protocol.into());
    map.insert("stubs".to_string(), Value::Array(stubs));
    for (k, v) in fields {
        map.insert(k.to_string(), v);
    }
    map
}

fn tls_fixture(name: &str) -> anyhow::Result<String> {
    let path = fixtures_dir().join("tls").join(name);
    std::fs::read_to_string(&path).with_context(|| format!("TLS fixture {}", path.display()))
}

fn test_ca_path() -> PathBuf {
    fixtures_dir().join("intercept").join("ca-cert.pem")
}

fn test_ca_pem() -> anyhow::Result<String> {
    let path = test_ca_path();
    std::fs::read_to_string(&path).with_context(|| format!("the test CA {}", path.display()))
}

fn cert_key(cert: &str, key: &str) -> anyhow::Result<Vec<(&'static str, Value)>> {
    Ok(vec![
        ("cert", tls_fixture(cert)?.into()),
        ("key", tls_fixture(key)?.into()),
    ])
}

/// Create an imposter on the shared engine, requiring a 201: for the setup steps, where a refusal
/// is a broken scenario. It becomes the scenario's target.
async fn create_tls_imposter(w: &mut PlaneA, map: serde_json::Map<String, Value>) -> Step {
    let json = Value::Object(map).to_string();
    let port = post_at_engine_port(&harness().admin_url, &json, "the HTTPS imposter").await?;
    w.tracked.push(port);
    w.current = Some(port);
    Ok(())
}

/// The scenario's `@tls-flags` engine.
fn flag_admin(w: &PlaneA) -> anyhow::Result<String> {
    w.side_engine
        .as_ref()
        .map(|e| e.admin_url.clone())
        .ok_or_else(|| anyhow!("no TLS-flag engine was started in this scenario"))
}

/// Create an imposter on the `@tls-flags` engine, requiring a 201. Not tracked: the engine and its
/// imposters are stopped after the scenario.
async fn create_on_flag_engine(w: &mut PlaneA, map: serde_json::Map<String, Value>) -> Step {
    let admin = flag_admin(w)?;
    let port = post_assigned(
        &admin,
        &Value::Object(map).to_string(),
        "the HTTPS imposter",
    )
    .await?;
    w.current = Some(port);
    Ok(())
}

/// POST an imposter and keep the answer as the last response, whatever it is. In container mode it
/// goes out on a pool port; a regression that accepts it has its port tracked.
async fn post_kept(w: &mut PlaneA, admin: &str, mut map: serde_json::Map<String, Value>) -> Step {
    let shared = admin == harness().admin_url;
    if shared && let Some(pool) = harness().imposter_port_pool() {
        let port = pool.next_port();
        w.tracked.push(port);
        map.insert("port".to_string(), port.into());
    }
    let json = Value::Object(map).to_string();
    let url = format!("{admin}/imposters");
    w.send_with(
        &clients().verifying,
        "POST",
        &url,
        &json_content_type(),
        Some(&json),
    )
    .await?;
    let resp = w.last()?;
    if resp.status == 201 {
        let port = declared_port(&resp.body, "the 201 for an imposter")?;
        if shared {
            w.tracked.push(port);
        }
        w.current = Some(port);
    }
    Ok(())
}

#[given(regex = r#"^an HTTPS imposter serving "((?:\\.|[^"\\])*)" with key "((?:\\.|[^"\\])*)"$"#)]
async fn https_imposter(w: &mut PlaneA, cert: String, key: String) -> Step {
    let fields = cert_key(&unescape(&cert), &unescape(&key))?;
    create_tls_imposter(w, tls_imposter("https", fields, vec![plain_tls_stub()])).await
}

#[given(
    regex = r#"^an HTTPS imposter serving the chain "((?:\\.|[^"\\])*)" then "((?:\\.|[^"\\])*)" with key "((?:\\.|[^"\\])*)"$"#
)]
async fn https_imposter_chain(
    w: &mut PlaneA,
    leaf: String,
    intermediate: String,
    key: String,
) -> Step {
    let chain = tls_fixture(&unescape(&leaf))? + tls_fixture(&unescape(&intermediate))?.as_str();
    let fields = vec![
        ("cert", chain.into()),
        ("key", tls_fixture(&unescape(&key))?.into()),
    ];
    create_tls_imposter(w, tls_imposter("https", fields, vec![plain_tls_stub()])).await
}

#[given(
    regex = r#"^an HTTPS imposter serving "((?:\\.|[^"\\])*)" with key "((?:\\.|[^"\\])*)" and "((?:\\.|[^"\\])*)"$"#
)]
async fn https_imposter_with_stubs(
    w: &mut PlaneA,
    cert: String,
    key: String,
    stubs: String,
) -> Step {
    let fields = cert_key(&unescape(&cert), &unescape(&key))?;
    let stubs = tls_stubs(&unescape(&stubs))?;
    create_tls_imposter(w, tls_imposter("https", fields, stubs)).await
}

/// GET `/` on the target imposter over HTTPS, offering `h2` then `http/1.1` and trusting only the
/// test CA. HTTP/2 is framed by hand (reqwest exposes neither the chain nor ALPN), so an `h2`
/// answer carries its status and body, not its headers.
async fn https_get_trusting_test_ca(w: &mut PlaneA) -> Step {
    let port = w.current()?;
    let host = harness().imposter_host();
    let config = tls::client_config(
        &tls::certs_from_pem(test_ca_pem()?.as_bytes())?,
        tls::ALPN_H2_FIRST,
    )?;
    w.curl.push(format!(
        "curl -sv --cacert {} --resolve localhost:{port}:{host} https://localhost:{port}/",
        test_ca_path().display()
    ));
    let tcp = wire::connect(host, port).await?;
    // `localhost`: every fixture leaf is issued for it, and the chain leaf for nothing else.
    let mut conn = tls::handshake(Box::new(tcp), "localhost", config).await?;
    let authority = format!("localhost:{port}");
    let (response, version) = if conn.alpn() == Some(b"h2".as_slice()) {
        match conn.h2_get("/", &authority).await? {
            H2Outcome::Answered {
                status: Some(status),
                body,
            } => (
                Response {
                    status,
                    body,
                    headers: BTreeMap::new(),
                },
                "HTTP_2",
            ),
            other => {
                return Err(anyhow!("the HTTP/2 GET / did not complete: {other:?}").into());
            }
        }
    } else {
        match conn
            .exchange(&wire::request("GET", &authority, "/", &[], b""))
            .await?
        {
            RawRead::Response(r) => (r.to_response(), "HTTP_1_1"),
            RawRead::Closed => {
                return Err(
                    anyhow!("the imposter closed the connection without answering GET /").into(),
                );
            }
        }
    };
    w.tls = Some(TlsExchange {
        chain: conn.peer_certificates().to_vec(),
        version,
    });
    w.last = Some(response);
    Ok(())
}

#[when(regex = r"^an HTTPS GET trusting only the test CA is sent to the imposter$")]
async fn https_get(w: &mut PlaneA) -> Step {
    https_get_trusting_test_ca(w).await
}

#[when(
    regex = r"^an HTTP/2-preferring HTTPS GET trusting only the test CA is sent to the imposter$"
)]
async fn https_get_h2(w: &mut PlaneA) -> Step {
    https_get_trusting_test_ca(w).await
}

impl PlaneA {
    fn tls(&self) -> anyhow::Result<&TlsExchange> {
        self.tls
            .as_ref()
            .ok_or_else(|| anyhow!("no HTTPS exchange yet in this scenario"))
    }
}

#[then(regex = r#"^the peer certificate subject is "((?:\\.|[^"\\])*)"$"#)]
async fn peer_subject(w: &mut PlaneA, expected: String) -> Step {
    let expected = unescape(&expected);
    let got = w.tls()?.chain.first().map(tls::subject).transpose()?;
    w.ensure(got.as_deref() == Some(expected.as_str()), || {
        format!("expected peer subject '{expected}', got {got:?}")
    })
}

#[then(
    regex = r#"^the peer certificate chain has (\d+) certificates, the first with subject "((?:\\.|[^"\\])*)"$"#
)]
async fn peer_chain(w: &mut PlaneA, n: usize, expected: String) -> Step {
    let expected = unescape(&expected);
    let subjects = w
        .tls()?
        .chain
        .iter()
        .map(tls::subject)
        .collect::<anyhow::Result<Vec<_>>>()?;
    w.ensure(
        subjects.len() == n && subjects.first() == Some(&expected),
        || format!("expected {n} certificates led by '{expected}', got {subjects:?}"),
    )
}

#[then(regex = r#"^the response was served over "((?:\\.|[^"\\])*)"$"#)]
async fn served_over(w: &mut PlaneA, expected: String) -> Step {
    let expected = unescape(&expected);
    let got = w.tls()?.version;
    w.ensure(got == expected, || {
        format!("expected {expected}, got {got}")
    })
}

#[when(regex = r#"^an imposter with "((?:\\.|[^"\\])*)" is posted$"#)]
async fn imposter_with_case(w: &mut PlaneA, kind: String) -> Step {
    let ca: Value = test_ca_pem()?.into();
    let yes = Value::Bool(true);
    let (protocol, fields) = match unescape(&kind).as_str() {
        "a key that does not match the cert" => {
            ("https", cert_key("server-leaf.pem", "chain-leaf.key")?)
        }
        "a cert without a key" => (
            "https",
            vec![("cert", tls_fixture("server-leaf.pem")?.into())],
        ),
        "rejectUnauthorized without mutualAuth" => {
            ("https", vec![("rejectUnauthorized", yes), ("ca", ca)])
        }
        "ca without rejectUnauthorized" => ("https", vec![("mutualAuth", yes), ("ca", ca)]),
        "rejectUnauthorized without ca" => (
            "https",
            vec![("mutualAuth", yes.clone()), ("rejectUnauthorized", yes)],
        ),
        "an empty ca list" => (
            "https",
            vec![
                ("mutualAuth", yes.clone()),
                ("rejectUnauthorized", yes),
                ("ca", Value::Array(Vec::new())),
            ],
        ),
        "a ca with no certificate" => (
            "https",
            vec![
                ("mutualAuth", yes.clone()),
                ("rejectUnauthorized", yes),
                ("ca", "no cert".into()),
            ],
        ),
        "mutualAuth on an http imposter" => ("http", vec![("mutualAuth", yes)]),
        "mutualAuth false on an http imposter" => {
            ("http", vec![("mutualAuth", Value::Bool(false))])
        }
        other => return Err(anyhow!("unknown imposter case '{other}'").into()),
    };
    let admin = harness().admin_url.clone();
    post_kept(
        w,
        &admin,
        tls_imposter(protocol, fields, vec![plain_tls_stub()]),
    )
    .await
}

#[given(regex = r#"^a plain-HTTP imposter answering 200 "((?:\\.|[^"\\])*)"$"#)]
async fn plain_http_imposter(w: &mut PlaneA, body: String) -> Step {
    let stub =
        serde_json::json!({"responses": [{"is": {"statusCode": 200, "body": unescape(&body)}}]});
    create_tls_imposter(w, tls_imposter("http", Vec::new(), vec![stub])).await
}

#[when(regex = r"^an h2c prior-knowledge GET is sent to the imposter$")]
async fn h2c_prior_knowledge_get(w: &mut PlaneA) -> Step {
    let port = w.current()?;
    let host = harness().imposter_host();
    w.curl.push(format!(
        "curl -sv --http2-prior-knowledge http://{host}:{port}/"
    ));
    let outcome = tokio::task::spawn_blocking(move || raw::h2c_get(host, port, "localhost", "/"))
        .await
        .context("the h2c task")??;
    w.h2c_get = Some(outcome);
    Ok(())
}

#[then(regex = r#"^the h2c response has status (\d+) and body "((?:\\.|[^"\\])*)"$"#)]
async fn h2c_response(w: &mut PlaneA, status: u16, body: String) -> Step {
    let want = H2Outcome::Answered {
        status: Some(status),
        body: unescape(&body),
    };
    let got = w
        .h2c_get
        .as_ref()
        .ok_or_else(|| anyhow!("no h2c exchange yet in this scenario"))?;
    w.ensure(*got == want, || {
        format!("expected an HTTP/2 {want:?}, got {got:?}")
    })
}

/// Start the scenario's `@tls-flags` engine, replacing any it already has.
async fn start_tls_flag_engine(w: &mut PlaneA, flags: &[String], env: &[(String, String)]) -> Step {
    if let Some(old) = w.side_engine.take() {
        old.stop().await?;
    }
    w.side_engine = Some(harness().spawn_flag_engine_with_env(flags, env).await?);
    Ok(())
}

#[given(
    regex = r#"^an engine started with the default TLS cert "((?:\\.|[^"\\])*)" and key "((?:\\.|[^"\\])*)"$"#
)]
async fn engine_with_default_tls(w: &mut PlaneA, cert: String, key: String) -> Step {
    let tls_dir = fixtures_dir().join("tls");
    let flags = [
        "--default-tls-cert".to_string(),
        tls_dir.join(unescape(&cert)).display().to_string(),
        "--default-tls-key".to_string(),
        tls_dir.join(unescape(&key)).display().to_string(),
    ];
    start_tls_flag_engine(w, &flags, &[]).await
}

#[given(regex = r#"^an engine started with "((?:\\.|[^"\\])*)"$"#)]
async fn engine_with_flag(w: &mut PlaneA, flag: String) -> Step {
    start_tls_flag_engine(w, &[unescape(&flag)], &[]).await
}

#[given(
    regex = r#"^an engine started with the environment "((?:\\.|[^"\\])*)" set to "((?:\\.|[^"\\])*)"$"#
)]
async fn engine_with_env(w: &mut PlaneA, name: String, value: String) -> Step {
    start_tls_flag_engine(w, &[], &[(unescape(&name), unescape(&value))]).await
}

#[given(regex = r"^an HTTPS imposter with no cert on that engine$")]
async fn https_no_cert_on_flag_engine(w: &mut PlaneA) -> Step {
    create_on_flag_engine(w, tls_imposter("https", Vec::new(), vec![plain_tls_stub()])).await
}

#[given(
    regex = r#"^an HTTPS imposter serving "((?:\\.|[^"\\])*)" with key "((?:\\.|[^"\\])*)" on that engine$"#
)]
async fn https_imposter_on_flag_engine(w: &mut PlaneA, cert: String, key: String) -> Step {
    let fields = cert_key(&unescape(&cert), &unescape(&key))?;
    create_on_flag_engine(w, tls_imposter("https", fields, vec![plain_tls_stub()])).await
}

#[when(regex = r"^an HTTPS imposter with no cert is posted to that engine$")]
async fn post_https_no_cert_to_flag_engine(w: &mut PlaneA) -> Step {
    let admin = flag_admin(w)?;
    post_kept(
        w,
        &admin,
        tls_imposter("https", Vec::new(), vec![plain_tls_stub()]),
    )
    .await
}

// Stricter than the Scala runner, which only saw the start fail: the engine must exit with 1 over
// the missing file without ever reporting a listener.
#[then(
    regex = r"^an engine started with a default TLS cert file that does not exist exits before serving$"
)]
async fn missing_default_cert_exits(w: &mut PlaneA) -> Step {
    let missing = harness().config.log_dir.join("no-such-default-cert.pem");
    if missing.exists() {
        return Err(anyhow!(
            "{} exists, so it cannot stand for a missing file",
            missing.display()
        )
        .into());
    }
    let flags = [
        "--default-tls-cert".to_string(),
        missing.display().to_string(),
        "--default-tls-key".to_string(),
        fixtures_dir()
            .join("tls/server-leaf.key")
            .display()
            .to_string(),
    ];
    w.curl.push(format!("rift {}", flags.join(" ")));
    let boot = harness().boot_engine(&flags, false).await?;
    let tail: Vec<&str> = boot.output.lines().rev().take(5).collect();
    w.ensure(
        boot.exit_code == Some(1)
            && boot.output.contains("No such file or directory")
            && !boot.output.contains("listening on"),
        || {
            format!(
                "expected the engine to exit with code 1 over the missing file before serving, got exit {:?}, output tail:\n{}",
                boot.exit_code,
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        },
    )
}

/// On a dedicated engine started with `flag`. The imposter is not tracked: the whole engine is
/// killed after the scenario.
#[when(
    regex = r#"^a proxy imposter to the HTTPS upstream is created on an engine started with "((?:\\.|[^"\\])*)"$"#
)]
async fn proxy_to_upstream_on_flag_engine(w: &mut PlaneA, flag: String) -> Step {
    let json = proxy_to_upstream_json(w)?;
    start_tls_flag_engine(w, &[unescape(&flag)], &[]).await?;
    let admin = flag_admin(w)?;
    let port = post_assigned(&admin, &json, "proxy to the HTTPS upstream (flag engine)").await?;
    w.current = Some(port);
    Ok(())
}

// ── the run ──────────────────────────────────────────────────────────────────────────────────

/// The two kinds of feature 12 engine a scenario owns, stopped alike.
enum Owned {
    Isolated(IsolatedEngine),
    Intercept(InterceptEngine),
}

impl Owned {
    fn log(&self) -> Option<&std::path::Path> {
        match self {
            Self::Isolated(e) => e.log(),
            Self::Intercept(e) => e.log(),
        }
    }

    async fn stop(self) -> Result<(), HarnessError> {
        match self {
            Self::Isolated(e) => e.stop().await,
            Self::Intercept(e) => e.stop().await,
        }
    }
}

/// After every scenario, even a failed one: delete its imposters from the shared engine and stop
/// any engine it started. A cleanup failure fails the run (as a hook error): a leaked imposter
/// squats its port for every later scenario.
async fn cleanup(world: Option<&mut PlaneA>) {
    let Some(w) = world else { return };
    let admin = &harness().admin_url;
    let mut errors = Vec::new();
    for port in w.tracked.drain(..) {
        if let Err(e) = delete_imposter(admin, port).await {
            errors.push(format!("{e:#}"));
        }
    }
    if let Some(engine) = w.side_engine.take() {
        let log = engine.log.clone();
        if let Err(e) = engine.stop().await {
            errors.push(format!("{e} (log {}:\n{})", log.display(), log_tail(&log)));
        }
    }
    if let Some(engine) = w.reload.take() {
        let log = engine.log().map(std::path::Path::to_path_buf);
        if let Err(e) = engine.stop().await {
            match log {
                Some(log) => {
                    errors.push(format!("{e} (log {}:\n{})", log.display(), log_tail(&log)));
                }
                None => errors.push(format!("stopping the embedded reload engine: {e}")),
            }
        }
    }
    if let Some(engine) = w.isolated.take() {
        let log = engine.log().map(std::path::Path::to_path_buf);
        if let Err(e) = engine.stop().await {
            match log {
                Some(log) => {
                    errors.push(format!("{e} (log {}:\n{})", log.display(), log_tail(&log)));
                }
                None => errors.push(format!("stopping the embedded isolated engine: {e}")),
            }
        }
    }
    // Raw connections first, so no tunnel outlives the engine it runs through.
    w.tunnel = None;
    w.listener_conn = None;
    w.ws_origin = None;
    if std::mem::take(&mut w.intercept_rules_added)
        && let Err(e) = clear_rules(admin).await
    {
        errors.push(format!("clearing the shared intercept rules: {e:#}"));
    }
    for (what, engine) in [
        (
            "runtime-intercept",
            w.runtime_engine.take().map(Owned::Isolated),
        ),
        ("intercept", w.intercept_engine.take().map(Owned::Intercept)),
    ] {
        let Some(engine) = engine else { continue };
        let log = engine.log().map(std::path::Path::to_path_buf);
        if let Err(e) = engine.stop().await {
            match log {
                Some(log) => {
                    errors.push(format!("{e} (log {}:\n{})", log.display(), log_tail(&log)));
                }
                None => errors.push(format!("stopping the embedded {what} engine: {e}")),
            }
        }
    }
    // A burst nobody joined must not outlive its scenario.
    if let Some(burst) = w.burst.take() {
        for request in burst.requests {
            request.abort();
        }
    }
    assert!(
        errors.is_empty(),
        "scenario cleanup failed: {}",
        errors.join("; ")
    );
}

/// Parses exactly the ported feature files, so a run covers only what this runner has ported.
/// Whether every feature parses at all is `features_parse.rs`'s gate, not this run's.
struct Ported;

impl Parser<Vec<PathBuf>> for Ported {
    type Cli = cli::Empty;
    type Output =
        futures::stream::Iter<std::vec::IntoIter<Result<gherkin::Feature, parser::Error>>>;

    fn parse(self, input: Vec<PathBuf>, _: cli::Empty) -> Self::Output {
        let features: Vec<_> = input
            .into_iter()
            .map(|path| common::parse_feature(&path))
            .collect();
        futures::stream::iter(features)
    }
}

/// Remove the still-empty log directory of a run that started no engine, so a plain
/// `cargo test --workspace` leaves nothing behind.
fn discard_empty(dir: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir(dir) {
        eprintln!("plane_a: removing {}: {e}", dir.display());
    }
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("plane_a: building the tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    install_crypto_provider();
    let required = env_set("RIFT_CONFORMANCE_REQUIRE");

    let log_dir = match tempfile::Builder::new()
        .prefix("rift-conformance-")
        .tempdir()
    {
        Ok(dir) => dir.keep(),
        Err(e) => {
            eprintln!("plane_a: creating the engine log directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = match Config::from_env(log_dir.clone()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            discard_empty(&log_dir);
            let msg = "RIFT_BUILD_DIR is not set, so there is no engine to run the Plane A \
                       features against. Stage a build (see conformance/README.md) and export \
                       RIFT_BUILD_DIR to run them.";
            return if required {
                eprintln!("plane_a: FAILED: {msg} (RIFT_CONFORMANCE_REQUIRE is set)");
                ExitCode::FAILURE
            } else {
                eprintln!("plane_a: SKIPPED, 0 scenarios run: {msg}");
                ExitCode::SUCCESS
            };
        }
        Err(e) => {
            discard_empty(&log_dir);
            eprintln!("plane_a: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mode = config.mode;

    match Clients::new() {
        Ok(c) => {
            if CLIENTS.set(c).is_err() {
                eprintln!("plane_a: the HTTP clients were already built");
                return ExitCode::FAILURE;
            }
        }
        Err(e) => {
            eprintln!("plane_a: {e}");
            return ExitCode::FAILURE;
        }
    }
    let started = match Harness::start(config).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!(
                "plane_a: starting the engine ({mode}): {e}\nengine logs: {}",
                log_dir.display()
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "plane_a: RIFT_MODE={mode}, admin {}, metrics {}, features {}, engine logs {}",
        started.admin_url,
        started.metrics_url,
        features_dir().display(),
        log_dir.display()
    );
    let started: &'static Harness = Box::leak(Box::new(started));
    let _teardown = Teardown(started);
    if HARNESS.set(started).is_err() {
        eprintln!("plane_a: the harness was already started");
        return ExitCode::FAILURE;
    }

    // cucumber's --name / --tags narrow the ported set rather than replace this filter.
    let mut cli = cli::Opts::<_, _, _, cli::Empty>::parsed();
    let name_filter = cli.re_filter.take();
    let tags_filter = cli.tags_filter.take();
    let excluded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let excluded_in_filter = std::rc::Rc::clone(&excluded);

    let ported: Vec<PathBuf> = PORTED_FEATURES
        .iter()
        .map(|f| features_dir().join(f))
        .collect();
    let writer = PlaneA::cucumber::<PathBuf>()
        .max_concurrent_scenarios(1)
        .fail_on_skipped()
        .after(|_, _, _, _, world| {
            SCENARIOS_RUN.fetch_add(1, Ordering::Relaxed);
            cleanup(world).boxed_local()
        })
        .with_parser(Ported)
        .with_cli(cli)
        .filter_run(ported, move |feature, rule, scenario| {
            let tags = || {
                feature
                    .tags
                    .iter()
                    .chain(rule.iter().flat_map(|r| &r.tags))
                    .chain(scenario.tags.iter())
            };
            if let Some(reason) = tags().find_map(|t| mode.excludes(t)) {
                excluded_in_filter
                    .borrow_mut()
                    .push(format!("{}: {} ({reason})", feature.name, scenario.name));
                return false;
            }
            name_filter
                .as_ref()
                .is_none_or(|re| re.is_match(&scenario.name))
                && tags_filter.as_ref().is_none_or(|op| op.eval(tags()))
        })
        .await;

    let shutdown = harness().shutdown().await;
    let excluded = excluded.borrow();
    if !excluded.is_empty() {
        eprintln!(
            "plane_a: {} scenario(s) not run in RIFT_MODE={mode}:\n  {}",
            excluded.len(),
            excluded.join("\n  ")
        );
    }
    let mut failed = writer.execution_has_failed();
    // A green step that ran nothing (a filter or exclusion that matched everything) proves nothing.
    if required && SCENARIOS_RUN.load(Ordering::Relaxed) == 0 {
        eprintln!("plane_a: FAILED: no scenario ran (RIFT_CONFORMANCE_REQUIRE is set)");
        failed = true;
    }
    if let Err(e) = &shutdown {
        eprintln!("plane_a: stopping the engine: {e}");
    }
    if failed || shutdown.is_err() {
        eprintln!("plane_a: engine logs kept in {}", log_dir.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::remove_dir_all(&log_dir) {
        eprintln!("plane_a: removing {}: {e}", log_dir.display());
    }
    ExitCode::SUCCESS
}
