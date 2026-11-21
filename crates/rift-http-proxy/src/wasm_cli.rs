//! `rift wasm check` (issue #1759): replay a saved imposter against a component, offline, and diff
//! each answer with the recorded one.
//!
//! The corpus is the saved imposter (`rift save`, `GET /imposters/:port?replayable=true`): its
//! stubs, in document order, are the recorded interactions. Each stub's request is synthesised from
//! its predicates by the generator rift-verify uses ([`crate::stub_requests`]); the component runs
//! in-process, with no server and no ports, against one in-memory flow store that lives for the
//! run, so a stateful component sees the sequence. A stub that carries `recordedAt` freezes the
//! clock there before its call; otherwise the clock is frozen at the start of the run.
//!
//! Exit codes, as `rift script check`: `0` no drift, `1` drift, `2` a usage, corpus or component
//! error.

use crate::server::WasmAction;
use crate::stub_requests::{TestCase, test_cases_for_stubs};
use crate::wasm_check_diff::{Answer, Field, Mismatch, default_fields, diff, parse_fields};
use anyhow::{Context, Result, anyhow, bail};
use rift_mock_core::backends::InMemoryFlowStore;
use rift_mock_core::imposter::ResponseMode;
use rift_mock_core::imposter::clock::{ImposterClock, wall_ms};
use rift_mock_core::imposter::rng::{ImposterRng, RequestRng, with_request_rng};
use rift_mock_core::scripting::{
    FaultDecision, ScriptClock, ScriptCtxExtras, ScriptRequest, WasmAnswer, WasmScript,
    should_inject_wasm,
};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A call's deadline. Generous: the first call compiles the component.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// What `rift wasm check` was asked to do.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub component: PathBuf,
    pub corpus: PathBuf,
    pub port: Option<u16>,
    pub export: Option<String>,
    pub knobs: Option<String>,
    pub seed: Option<u64>,
    pub fields: Option<String>,
    pub stop_on_first: bool,
}

/// One stub whose answer drifted from the recording.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Drift {
    pub stub_index: usize,
    /// Which of the stub's recorded responses (a `proxyAlways` stub holds one per call).
    pub response_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<String>,
    pub mismatches: Vec<Mismatch>,
}

/// One stub not replayed, and why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skip {
    pub stub_index: usize,
    pub reason: String,
}

/// The run's result; `--json` prints it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckReport {
    pub component: String,
    pub corpus: String,
    pub port: Option<u16>,
    pub fields: Vec<String>,
    pub stubs: usize,
    pub checked: usize,
    pub skipped: usize,
    pub drifted: usize,
    pub drifts: Vec<Drift>,
    pub skips: Vec<Skip>,
}

impl CheckReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.drifted == 0
    }
}

/// The imposter the corpus names: a single imposter document, or `{"imposters": [...]}` with
/// `port` choosing one (optional when there is exactly one).
fn pick_imposter(corpus: Value, port: Option<u16>) -> Result<Value> {
    let Some(imposters) = corpus.get("imposters").and_then(Value::as_array) else {
        return Ok(corpus);
    };
    let port_of = |i: &Value| i.get("port").and_then(Value::as_u64);
    match port {
        Some(p) => imposters
            .iter()
            .find(|i| port_of(i) == Some(u64::from(p)))
            .cloned()
            .ok_or_else(|| anyhow!("the corpus has no imposter on port {p}")),
        None => match imposters.as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!("the corpus has no imposters"),
            many => bail!(
                "the corpus has {} imposters; choose one with --port ({})",
                many.len(),
                many.iter()
                    .filter_map(port_of)
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// The flow a request belongs to: `_rift.flowState.flowIdSource` `header:<Name>` when the request
/// carries that header, else the imposter's port, as the server resolves it.
fn flow_id(imposter: &Value, case: &TestCase) -> String {
    let port = imposter.get("port").and_then(Value::as_u64).unwrap_or(0);
    imposter
        .pointer("/_rift/flowState/flowIdSource")
        .and_then(Value::as_str)
        .and_then(|source| source.strip_prefix("header:"))
        .and_then(|name| {
            case.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| port.to_string())
}

fn request_of(case: &TestCase) -> ScriptRequest {
    let mut headers = rift_mock_core::util::MultiValueMap::default();
    for (k, v) in &case.headers {
        headers.insert(k.to_ascii_lowercase(), vec![v.clone()]);
    }
    let mut query = rift_mock_core::util::MultiValueMap::default();
    for (k, v) in &case.query_params {
        query.insert(k.clone(), vec![v.clone()]);
    }
    let body_text = case.body.clone().unwrap_or_default();
    ScriptRequest {
        method: case.method.clone(),
        path: case.path.clone(),
        headers,
        // Domain-optional parse: the script contract exposes a non-JSON body as `null`.
        body: serde_json::from_str(&body_text).unwrap_or(Value::Null),
        query,
        path_params: Default::default(),
        raw_body: Some(body_text),
        mode: ResponseMode::Text,
    }
}

fn expected_of(case: &TestCase) -> Answer {
    Answer {
        status: case.expected_status,
        headers: case
            .expected_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        body: match &case.expected_body {
            None => Vec::new(),
            Some(Value::String(s)) => s.clone().into_bytes(),
            Some(other) => other.to_string().into_bytes(),
        },
    }
}

/// The component's answer, or the mismatch that says it gave none. An http component's repeated
/// header values are joined with `", "`, as a recording stores one value per name.
fn answer_of(result: Result<WasmAnswer>) -> Result<Answer, Mismatch> {
    let not_an_answer = |actual: Value| Mismatch {
        field: "decision".into(),
        expected: Value::from("respond"),
        actual,
    };
    let result = match result {
        Ok(WasmAnswer::Response(response)) => {
            return Ok(Answer {
                status: response.status,
                headers: rift_mock_core::scripting::joined_headers(&response.headers)
                    .into_iter()
                    .collect(),
                body: response.body.to_vec(),
            });
        }
        Ok(WasmAnswer::Decision(decision)) => Ok(decision),
        Err(e) => Err(e),
    };
    match result {
        Ok(FaultDecision::Error {
            status,
            body,
            headers,
            ..
        }) => Ok(Answer {
            status,
            headers: headers.into_iter().collect(),
            body: body.into_bytes(),
        }),
        Ok(FaultDecision::None) => Err(not_an_answer(Value::from("pass"))),
        Ok(FaultDecision::Latency { duration_ms, .. }) => {
            Err(not_an_answer(Value::from(format!("delay({duration_ms})"))))
        }
        Ok(FaultDecision::Reset { .. }) => Err(not_an_answer(Value::from("reset"))),
        Err(e) => Err(Mismatch {
            field: "error".into(),
            expected: Value::Null,
            actual: Value::from(format!("{e:#}")),
        }),
    }
}

/// Run the check. `Err` is a usage, corpus or component error (exit 2); drift is in the report.
pub async fn run_check(opts: &CheckOptions) -> Result<CheckReport> {
    match opts.export.as_deref() {
        None | Some("rift" | "http") => {}
        Some(other) => bail!("--export must be \"rift\" or \"http\" (got {other:?})"),
    }
    let fields: Vec<Field> = match &opts.fields {
        Some(spec) => parse_fields(spec).map_err(|e| anyhow!(e))?,
        None => default_fields(),
    };
    let knobs: Option<Value> = opts
        .knobs
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("--knobs is not JSON")?;

    let bytes = std::fs::read(&opts.component)
        .with_context(|| format!("cannot read the component {}", opts.component.display()))?;
    let code = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    // Admission: a broken component, or one without the export asked for, is an error for the run,
    // not drift on every stub.
    let export = rift_mock_core::scripting::admit_wasm_component(&code, opts.export.as_deref())
        .map_err(|e| anyhow!("{}: {e}", opts.component.display()))?;
    // A recorded binary body can match only an http component's bytes (issue #1758): the export
    // that answers, whether `--export` named it or the component's own exports decided it.
    let http_export = export == "http";

    let corpus_text = std::fs::read_to_string(&opts.corpus)
        .with_context(|| format!("cannot read the corpus {}", opts.corpus.display()))?;
    let corpus: Value = serde_json::from_str(&corpus_text)
        .with_context(|| format!("the corpus {} is not JSON", opts.corpus.display()))?;
    let imposter = pick_imposter(corpus, opts.port)?;
    let stubs = imposter
        .get("stubs")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "the corpus {} has no \"stubs\" array: is it a saved imposter (rift save)?",
                opts.corpus.display()
            )
        })?;
    let protocol = imposter
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("http");
    let (expanded, origins) = one_response_per_case(&stubs);
    let mut cases = test_cases_for_stubs(&expanded, protocol, None).map_err(|e| anyhow!(e))?;
    // One case per expanded stub, in order: point each back at the corpus's own stub index (its
    // response index stays in `origins`, by position).
    for case in &mut cases {
        case.stub_index = origins[case.stub_index].0;
    }
    let engine = imposter.pointer("/_rift/scriptEngine");
    // The corpus's own engine settings, as the server would run them; `--seed` overrides its seed.
    let seed = match opts.seed {
        Some(seed) => Some(seed),
        None => engine.and_then(|e| e.get("seed")).and_then(Value::as_u64),
    };
    let limits: rift_mock_core::imposter::WasmLimits = engine
        .and_then(|e| e.get("wasm"))
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("the corpus's _rift.scriptEngine.wasm")?
        .unwrap_or_default();
    let timeout = engine
        .and_then(|e| e.get("timeoutMs"))
        .and_then(Value::as_u64)
        .map_or(CALL_TIMEOUT, |ms| {
            Duration::from_millis(ms).max(CALL_TIMEOUT)
        });

    let store: Arc<dyn rift_mock_core::extensions::flow_state::FlowStore> =
        Arc::new(InMemoryFlowStore::new(300));
    let clock = Arc::new(ImposterClock::default());
    let rng = Arc::new(ImposterRng::new(seed));
    // The corpus's `rngKey` (issue #1795), as the server applies it: each request whose key parts
    // are present draws from its own stream. A key admission would refuse is an error here.
    let key_parts = match engine.and_then(|e| e.get("rngKey")) {
        None => None,
        Some(raw) => {
            let key: rift_mock_core::imposter::rng::RngKey = serde_json::from_value(raw.clone())
                .context("the corpus's _rift.scriptEngine.rngKey")?;
            let parts = key.parts().map_err(|e| anyhow!(e))?;
            if seed.is_none() {
                bail!(
                    "the corpus's _rift.scriptEngine.rngKey needs a seed (--seed or scriptEngine.seed)"
                );
            }
            Some(parts)
        }
    };
    let script_clock = ScriptClock::Imposter(Arc::clone(&clock));
    // The first recorded instant, else the start of the run; a stub without `recordedAt` keeps the
    // instant before it, so the clock never jumps forward to the run and back.
    let mut instant = match stubs.iter().find_map(|s| s.get("recordedAt")) {
        Some(at) => recorded_ms(at)?,
        None => wall_ms(),
    };

    let mut report = CheckReport {
        component: opts.component.display().to_string(),
        corpus: opts.corpus.display().to_string(),
        port: imposter
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok()),
        fields: fields.iter().map(Field::name).collect(),
        stubs: stubs.len(),
        checked: 0,
        skipped: 0,
        drifted: 0,
        drifts: Vec::new(),
        skips: Vec::new(),
    };
    let mut skipped_stubs = std::collections::BTreeSet::new();
    for (n, case) in cases.iter().enumerate() {
        let response_index = origins[n].1;
        let reason = skip_reason(case).or_else(|| {
            if http_export {
                None
            } else {
                binary_recording(&stubs[case.stub_index], response_index)
            }
        });
        if let Some(reason) = reason {
            if skipped_stubs.insert(case.stub_index) {
                report.skipped += 1;
                report.skips.push(Skip {
                    stub_index: case.stub_index,
                    reason,
                });
            }
            continue;
        }
        if let Some(at) = stubs[case.stub_index].get("recordedAt") {
            instant = recorded_ms(at).map_err(|e| anyhow!("stub {}: {e}", case.stub_index))?;
        }
        clock
            .freeze(instant)
            .map_err(|e| anyhow!("stub {}: recordedAt: {e}", case.stub_index))?;
        let request = request_of(case);
        let extras = ScriptCtxExtras {
            flow_id: Some(flow_id(&imposter, case)),
            port: report.port.unwrap_or(0),
            ..Default::default()
        };
        let handle = RequestRng::new(Arc::clone(&rng));
        if let (Some(parts), Some(seed)) = (&key_parts, seed) {
            let header = |name: &str| {
                case.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            };
            let query = |name: &str| case.query_params.get(name).cloned();
            let source = rift_mock_core::imposter::rng::KeySource {
                method: &case.method,
                path: &case.path,
                header: &header,
                query: &query,
                body: case.body.as_deref(),
            };
            if let Some(digest) = rift_mock_core::imposter::rng::key_digest(parts, &source) {
                let occurrence = rng.next_occurrence(digest);
                handle.rekey(ImposterRng::new(Some(
                    rift_mock_core::imposter::rng::keyed_state(seed, digest, occurrence),
                )));
            }
        }
        let result = with_request_rng(
            handle,
            should_inject_wasm(
                WasmScript {
                    code: &code,
                    export: Some(export),
                    knobs: knobs.as_ref(),
                    limits,
                    clock: Some(&script_clock),
                },
                "wasm-check",
                &request,
                Arc::clone(&store),
                timeout,
                &extras,
            ),
        )
        .await;
        report.checked += 1;
        let mismatches = match answer_of(result) {
            Ok(actual) => {
                let mut expected = expected_of(case);
                if http_export
                    && let Some(bytes) = binary_body(&stubs[case.stub_index], response_index)
                        .map_err(|e| anyhow!("stub {}: {e}", case.stub_index))?
                {
                    expected.body = bytes;
                }
                diff(&fields, &expected, &actual)
            }
            Err(not_an_answer) => vec![not_an_answer],
        };
        if !mismatches.is_empty() {
            report.drifted += 1;
            report.drifts.push(Drift {
                stub_index: case.stub_index,
                response_index,
                stub_id: case.stub_id.clone(),
                mismatches,
            });
            if opts.stop_on_first {
                break;
            }
        }
    }
    if report.checked == 0 && report.drifted == 0 {
        bail!(
            "nothing to check: {} stubs, all skipped ({})",
            report.stubs,
            report
                .skips
                .iter()
                .map(|s| format!("stub {}: {}", s.stub_index, s.reason))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(report)
}

/// A stub per recorded response, so a `proxyAlways` stub (one `is` per recorded call, in order)
/// replays every call; a stub with any other response shape is kept whole, for the generator's own
/// skip reasons. Returns the expanded stubs and, for each, `(stub index, response index)`.
fn one_response_per_case(stubs: &[Value]) -> (Vec<Value>, Vec<(usize, usize)>) {
    let mut expanded = Vec::new();
    let mut origins = Vec::new();
    for (i, stub) in stubs.iter().enumerate() {
        let responses = stub.get("responses").and_then(Value::as_array);
        match responses {
            Some(responses)
                if responses.len() > 1 && responses.iter().all(|r| r.get("is").is_some()) =>
            {
                for (r, response) in responses.iter().enumerate() {
                    let mut one = stub.clone();
                    one["responses"] = Value::Array(vec![response.clone()]);
                    expanded.push(one);
                    origins.push((i, r));
                }
            }
            _ => {
                expanded.push(stub.clone());
                origins.push((i, 0));
            }
        }
    }
    (expanded, origins)
}

/// `recordedAt` (seconds since the epoch) in milliseconds; anything else is a corpus error.
fn recorded_ms(at: &Value) -> Result<i64> {
    at.as_i64()
        .and_then(|secs| secs.checked_mul(1000))
        .ok_or_else(|| anyhow!("recordedAt must be whole seconds since the Unix epoch (got {at})"))
}

/// Whether the recorded response is a binary body (`_mode: binary`).
fn is_binary(stub: &Value, response_index: usize) -> bool {
    stub.pointer(&format!("/responses/{response_index}/is/_mode"))
        .and_then(Value::as_str)
        == Some("binary")
}

/// A binary recording's bytes, decoded from its own base64: the test-case generator decodes only
/// a body that is UTF-8, and an http component answers any bytes (issue #1758).
fn binary_body(stub: &Value, response_index: usize) -> Result<Option<Vec<u8>>> {
    use base64::Engine as _;
    if !is_binary(stub, response_index) {
        return Ok(None);
    }
    let text = stub
        .pointer(&format!("/responses/{response_index}/is/body"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map(Some)
        .map_err(|e| anyhow!("a binary recorded body is not base64: {e}"))
}

/// A recorded binary body under the `rift` export, which answers only UTF-8 bodies, so it could
/// never match; `--export http` checks it (issue #1758).
fn binary_recording(stub: &Value, response_index: usize) -> Option<String> {
    is_binary(stub, response_index).then(|| {
        "a binary recorded body; the rift export answers UTF-8 bodies only (use --export http)"
            .into()
    })
}

/// Why a case is not replayed: what rift-verify skips (inject, proxy, script and fault stubs,
/// predicates it cannot synthesise), a no-match stub, or an expected transport error.
fn skip_reason(case: &TestCase) -> Option<String> {
    if let Some(reason) = &case.skip_reason {
        return Some(reason.clone());
    }
    if case.is_no_match_stub {
        return Some("matches no request (a catch-all or no-match stub)".into());
    }
    if case.expects_transport_error {
        return Some("expects a connection fault, not an answer".into());
    }
    None
}

fn print_report(report: &CheckReport) {
    println!(
        "rift wasm check: {} against {}{}",
        report.component,
        report.corpus,
        report
            .port
            .map(|p| format!(" (port {p})"))
            .unwrap_or_default()
    );
    for drift in &report.drifts {
        let stub = match &drift.stub_id {
            Some(id) => format!("stub {} ({id})", drift.stub_index),
            None => format!("stub {}", drift.stub_index),
        };
        for m in &drift.mismatches {
            println!(
                "  DRIFT {stub} {}: expected {}, got {}",
                m.field, m.expected, m.actual
            );
        }
    }
    for skip in &report.skips {
        println!("  skipped stub {}: {}", skip.stub_index, skip.reason);
    }
    println!(
        "{} stubs, {} checked, {} skipped, {} drifted",
        report.stubs, report.checked, report.skipped, report.drifted
    );
}

/// Handle `rift wasm <action>`: run it, print the report (`--json`: the report as JSON), and exit
/// `0`, `1` on drift, or `2` on an error that kept the check from running.
pub fn dispatch(action: WasmAction) -> ! {
    let WasmAction::Check {
        component,
        corpus,
        port,
        export,
        knobs,
        seed,
        fields,
        stop_on_first,
        json,
    } = action;
    let opts = CheckOptions {
        component,
        corpus,
        port,
        export,
        knobs,
        seed,
        fields,
        stop_on_first,
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "error": format!("cannot start a runtime: {e}") })
                );
            } else {
                eprintln!("rift wasm check: cannot start a runtime: {e}");
            }
            std::process::exit(2);
        }
    };
    match runtime.block_on(run_check(&opts)) {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(e) => {
                        eprintln!("rift wasm check: cannot render the report: {e}");
                        std::process::exit(2);
                    }
                }
            } else {
                print_report(&report);
            }
            std::process::exit(if report.is_ok() { 0 } else { 1 });
        }
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({ "error": format!("{e:#}") }));
            } else {
                eprintln!("rift wasm check: {e:#}");
            }
            std::process::exit(2);
        }
    }
}
