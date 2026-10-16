//! Plane A conformance (issue #1346): the features in `conformance/features/rift/` that this runner
//! has ported (`spec::PORTED_FEATURES`), run against a real engine.
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
//! Step text is byte-identical to the Scala runner's (`RiftConformanceSupport.scala`), so a feature
//! file means the same thing to both while features move from one runner to the other.

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
    Config, Harness, IsolatedEngine, ReloadEngine, SpawnedEngine, free_port, log_tail, port_taken,
};
use rift_conformance_runner::http::{
    Clients, FailureKind, Response, classify_failure, install_crypto_provider,
};
use rift_conformance_runner::prom::{self, PromText, Series};
use rift_conformance_runner::raw::{self, H2_INTERNAL_ERROR, H2Outcome, RawCapture};
use rift_conformance_runner::spec::{PORTED_FEATURES, features_dir, fixtures_dir};
use rift_conformance_runner::tools::{CORPUS_VERIFY_ARGS, binary_version};
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
    let port = reload_engine(w)?
        .front_door_port
        .ok_or_else(|| anyhow!("no front-door engine started in this scenario"))?;
    let url = format!(
        "http://{}:{port}{}",
        harness().imposter_host(),
        unescape(&path)
    );
    let headers = vec![("Host".to_string(), unescape(&host))];
    w.send_with(&clients().verifying, &method, &url, &headers, None)
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
    w.curl.push(format!(
        "printf '{method} {path} HTTP/1.1\\r\\nHost: {host}\\r\\n\\r\\n' | {}",
        if tls {
            format!("openssl s_client -quiet -connect {host}:{port}")
        } else {
            format!("nc {host} {port}")
        }
    ));
    let cap = tokio::task::spawn_blocking(move || {
        raw::raw_capture(&host, port, &method, &path, &host, tls)
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

/// The engine assigns a port-less imposter the first free port at or above this (#199).
const AUTO_PORT_FLOOR: u16 = 49152;

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
    w.tracked.extend(declared);
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
    let auto: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|p| *p >= AUTO_PORT_FLOOR)
        .collect();
    w.ensure(auto.len() == 1, || {
        format!("expected one auto-assigned port, got imposters {ports:?}")
    })?;
    w.current = auto.first().copied();
    Ok(())
}

#[then(regex = r"^the assigned port is at least 49152$")]
async fn assigned_port_floor(w: &mut PlaneA) -> Step {
    let port = w.current()?;
    w.ensure(port >= AUTO_PORT_FLOOR, || {
        format!("expected an auto-assigned port >= {AUTO_PORT_FLOOR}, got {port}")
    })
}

// ── the run ──────────────────────────────────────────────────────────────────────────────────

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
