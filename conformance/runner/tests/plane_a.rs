//! Plane A conformance (issue #1346): the features in `conformance/features/rift/` that this runner
//! has ported (`spec::PORTED_FEATURES`), run against a real engine.
//!
//! - `RIFT_BUILD_DIR` (required): `rift`, `rift-lint`, `rift-verify` and `BUILD_INFO`, staged as
//!   `.github/workflows/conformance.yml` does. Unset, the run prints why and passes, unless
//!   `RIFT_CONFORMANCE_REQUIRE` is set (CI sets it), when it fails.
//! - `RIFT_MODE`: `external` (default; spawn the staged binary), `embedded` (link rift-ffi and serve
//!   the admin API in this process) or `container` (not implemented yet: reported, not run).
//! - `RIFT_CORPUS_ROOT`, `RIFT_FEATURES_DIR`, `RIFT_MAX_BODY_SIZE`: as for the Scala runner.
//!
//! cucumber's own flags still work after `--`: `--name <regex>`, `--tags <expr>`, `--fail-fast`.
//! They narrow the ported set; they never widen it past it or past what the mode can run.
//!
//! Step text is byte-identical to the Scala runner's (`RiftConformanceSupport.scala`), so a feature
//! file means the same thing to both while features move from one runner to the other.

use anyhow::{Context as _, anyhow, bail};
use cucumber::feature::Ext as _;
use cucumber::parser::{self, Parser};
use cucumber::tag::Ext as _;
use cucumber::writer::Stats as _;
use cucumber::{World as _, cli, gherkin, given, then, when};
use futures::FutureExt as _;
use regex::Regex;
use rift_conformance_runner::HarnessError;
use rift_conformance_runner::harness::{Config, Harness, Mode, SpawnedEngine, log_tail};
use rift_conformance_runner::http::{
    Clients, FailureKind, Response, classify_failure, install_crypto_provider,
};
use rift_conformance_runner::spec::{PORTED_FEATURES, features_dir, fixtures_dir};
use rift_conformance_runner::tools::{CORPUS_VERIFY_ARGS, binary_version};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Duration;

/// The steps' view of the harness `run()` owns. `run()` leaks it to get the `&'static` the step
/// functions need, and holds a [`Teardown`] guard so the engine is still stopped when the run
/// panics outside cucumber's per-step catch (a static is never dropped, so it cannot do that).
static HARNESS: OnceLock<&'static Harness> = OnceLock::new();
static CLIENTS: OnceLock<Clients> = OnceLock::new();

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
    /// A dedicated engine this scenario started (feature 09 `@upstream-trust`), killed after it.
    side_engine: Option<SpawnedEngine>,
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

    fn record_curl(&mut self, method: &str, url: &str) {
        let mut line = format!("curl -X {}", method.to_uppercase());
        for (k, v) in &self.pending.headers {
            line.push_str(&format!(" -H '{k}: {v}'"));
        }
        if let Some(b) = &self.pending.body {
            line.push_str(&format!(" -d '{b}'"));
        }
        line.push_str(&format!(" '{url}'"));
        self.curl.push(line);
    }

    /// Send the staged request with `client` and keep the response.
    async fn send(&mut self, client: &reqwest::Client, method: &str, url: &str) -> Step {
        self.record_curl(method, url);
        let method_value = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
            .with_context(|| format!("HTTP method {method:?}"))?;
        let mut request = client.request(method_value, url);
        for (k, v) in &self.pending.headers {
            request = request.header(k, v);
        }
        if let Some(body) = &self.pending.body {
            request = request.body(body.clone());
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("transport failure on {method} {url}"))?;
        self.last = Some(
            Response::read(resp)
                .await
                .with_context(|| format!("reading the response of {method} {url}"))?,
        );
        Ok(())
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
/// (read back from the 201). No probe-then-bind window: the engine binds the port it reports.
async fn post_at_engine_port(admin: &str, raw: &str, what: &str) -> anyhow::Result<u16> {
    let mut map = parse_object(raw, what)?;
    map.remove("port");
    let (status, body) = post_imposter(admin, &Value::Object(map).to_string()).await?;
    if status != 201 {
        bail!("POST imposter ({what}) returned {status}, expected 201: {body}");
    }
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
        let connect = tokio::net::TcpStream::connect(("localhost", port));
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

// ── the run ──────────────────────────────────────────────────────────────────────────────────

/// After every scenario, even a failed one: delete its imposters from the shared engine and kill
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
    assert!(
        errors.is_empty(),
        "scenario cleanup failed: {}",
        errors.join("; ")
    );
}

/// Parses exactly the ported feature files. cucumber's own parser walks a whole directory, and a
/// feature this runner has not ported yet is not its business: gherkin-rs not parsing one
/// (today 04 and 05) must not fail this run. A ported file that does not parse still fails it.
struct Ported;

impl Parser<Vec<PathBuf>> for Ported {
    type Cli = cli::Empty;
    type Output =
        futures::stream::Iter<std::vec::IntoIter<Result<gherkin::Feature, parser::Error>>>;

    fn parse(self, input: Vec<PathBuf>, _: cli::Empty) -> Self::Output {
        let features: Vec<_> = input
            .into_iter()
            .map(|path| {
                gherkin::Feature::parse_path(path, gherkin::GherkinEnv::default())
                    .map_err(parser::Error::from)
                    .and_then(|f| f.expand_examples().map_err(parser::Error::from))
            })
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
    if mode == Mode::Container {
        discard_empty(&log_dir);
        let msg = "RIFT_MODE=container is not implemented in this runner yet (follow-up to \
                   rift#1346: the image-run engine behind 1:1 published ports, as the Scala \
                   runner's RiftHarnessContainer). Use RIFT_MODE=external or embedded.";
        return if required {
            eprintln!("plane_a: FAILED: {msg}");
            ExitCode::FAILURE
        } else {
            eprintln!("plane_a: SKIPPED, 0 scenarios run: {msg}");
            ExitCode::SUCCESS
        };
    }

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
        .after(|_, _, _, _, world| cleanup(world).boxed_local())
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
    let failed = writer.execution_has_failed();
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
