//! The process-global wasmtime engine, the linker that sets the sandbox, the component cache, and
//! one call of a component's `respond`.

use super::WasmAdmitError;
use super::host::bindings::rift::mock::types::{Decision, Header, Request};
use super::host::bindings::{Mock, MockPre};
use super::host::{FIELD_SIZE_BYTES, Host, Limits, NoOutgoing};
use super::pipe::LogPipe;
use crate::extensions::flow_state::FlowStore;
use crate::imposter::{ResponseMode, WasmLimits};
use crate::scripting::{
    FaultDecision, ScriptClock, ScriptCtxExtras, ScriptRequest, ScriptResult, ScriptResultBody,
    ScriptTimeoutError, WasmAnswer, WasmResponse, WasmScript,
};
use anyhow::anyhow;
use base64::Engine as _;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder, Trap, UpdateDeadline};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView as _};

/// How often the epoch ticks: a CPU-bound guest yields to tokio, and sees its deadline, this often.
const EPOCH_TICK: Duration = Duration::from_millis(10);
/// Core instances one store may create. A component is several (the guest module plus its wasip2
/// adapter shims), so 1 refuses every real one.
const MAX_INSTANCES: usize = 16;
/// Bytes kept of a call's stdout and of its stderr; the rest is dropped, not an error (`LogPipe`).
const LOG_PIPE_BYTES: usize = 64 * 1024;
/// Distinct components kept compiled. Full ⇒ cleared before the next insert, as the Rhai cache.
const CACHE_CAPACITY: usize = 256;
/// Resources (streams, pollables, …) one call may hold at once.
const MAX_RESOURCES: usize = 1024;

static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();

/// The one engine every component compiles and runs on, and its epoch ticker.
fn engine() -> anyhow::Result<&'static Engine> {
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model(true)
                .consume_fuel(true)
                .epoch_interruption(true)
                .cranelift_nan_canonicalization(true)
                .wasm_threads(false)
                // Proposals a wasm32-wasip2 component does not use: less for a guest to reach.
                .wasm_gc(false)
                .wasm_memory64(false)
                .relaxed_simd_deterministic(true);
            let engine = Engine::new(&config).map_err(|e| format!("{e:#}"))?;
            let ticking = engine.clone();
            std::thread::Builder::new()
                .name("rift-wasm-epoch".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(EPOCH_TICK);
                        ticking.increment_epoch();
                    }
                })
                .map_err(|e| format!("cannot start the wasm epoch ticker: {e}"))?;
            Ok(engine)
        })
        .as_ref()
        .map_err(|e| anyhow!("wasm engine unavailable: {e}"))
}

/// The interfaces a component may import. Everything a wasm32-wasip2 std guest imports, with
/// nothing behind it, `rift:mock/state`, and `wasi:http/types` (issue #1758); never wasi:sockets or
/// `wasi:http/outgoing-handler`, so no component can reach the network.
fn build_linker(engine: &Engine) -> wasmtime::Result<Linker<Host>> {
    use wasmtime_wasi::p2::bindings::{cli, filesystem, random};
    use wasmtime_wasi::{cli::WasiCli, filesystem::WasiFilesystem, random::WasiRandom};
    use wasmtime_wasi::{cli::WasiCliView, filesystem::WasiFilesystemView, random::WasiRandomView};
    let mut l = Linker::new(engine);
    // io/{error,poll,streams}, clocks/{wall-clock,monotonic-clock}, random/random, cli/std{in,out,err}.
    wasmtime_wasi::p2::add_to_linker_proxy_interfaces_async(&mut l)?;
    cli::exit::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::environment::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::terminal_input::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::terminal_output::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::terminal_stdin::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::terminal_stdout::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    cli::terminal_stderr::add_to_linker::<Host, WasiCli>(&mut l, Host::cli)?;
    filesystem::types::add_to_linker::<Host, WasiFilesystem>(&mut l, Host::filesystem)?;
    filesystem::preopens::add_to_linker::<Host, WasiFilesystem>(&mut l, Host::filesystem)?;
    random::insecure::add_to_linker::<Host, WasiRandom>(&mut l, Host::random)?;
    random::insecure_seed::add_to_linker::<Host, WasiRandom>(&mut l, Host::random)?;
    Mock::add_to_linker::<_, HasSelf<Host>>(&mut l, |h| h)?;
    // `wasi:http/types` alone, not `p2::add_only_http_to_linker_async`, which also links the
    // outgoing handler.
    wasmtime_wasi_http::p2::bindings::http::types::add_to_linker::<
        Host,
        wasmtime_wasi_http::WasiHttp,
    >(
        &mut l,
        &wasmtime_wasi_http::p2::bindings::LinkOptions::default().into(),
        Host::http,
    )?;
    Ok(l)
}

static LINKER: OnceLock<Result<Linker<Host>, String>> = OnceLock::new();

fn linker() -> anyhow::Result<&'static Linker<Host>> {
    LINKER
        .get_or_init(|| {
            let engine = engine().map_err(|e| format!("{e:#}"))?;
            build_linker(engine).map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("wasm linker unavailable: {e}"))
}

/// A compiled, linked component, ready to instantiate.
struct Loaded {
    /// The base64 text it was loaded from, compared on every hit: the key is only a fingerprint.
    code: Arc<str>,
    /// The first 12 hex digits of the bytes' SHA-256, for logs.
    sha12: Arc<str>,
    /// What the component imports, by interface name (issue #1757 reports them on upload).
    imports: Arc<[String]>,
    /// `rift:mock/handler`, or why the component does not export it as Rift expects.
    mock: Result<MockPre<Host>, Arc<str>>,
    /// `wasi:http/incoming-handler` (issue #1758), or why the component does not export it.
    proxy: Result<ProxyPre<Host>, Arc<str>>,
}

/// Which export of a component answers (`_rift.script.export`, issue #1758).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasmExport {
    /// `rift:mock/handler`: `init(knobs)`, then `respond` returns a decision.
    Rift,
    /// `wasi:http/incoming-handler`: the component answers the whole response.
    Http,
}

impl WasmExport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rift => "rift",
            Self::Http => "http",
        }
    }

    fn interface(self) -> &'static str {
        match self {
            Self::Rift => "rift:mock/handler",
            Self::Http => "wasi:http/incoming-handler",
        }
    }

    /// `export` as written, without the component: an unknown name is refused before compiling.
    fn parse(requested: Option<&str>) -> Result<Option<Self>, WasmAdmitError> {
        match requested {
            None => Ok(None),
            Some("rift") => Ok(Some(Self::Rift)),
            Some("http") => Ok(Some(Self::Http)),
            Some(other) => Err(WasmAdmitError::Export(other.to_owned())),
        }
    }

    /// The export that answers for `loaded`. Unset is `rift` when the component exports
    /// `rift:mock/handler`, else `http`; a named export the component lacks is refused, naming
    /// what it does export.
    fn resolve(requested: Option<&str>, loaded: &Loaded) -> Result<Self, WasmAdmitError> {
        let exports = |e: Self| match e {
            Self::Rift => loaded.mock.is_ok(),
            Self::Http => loaded.proxy.is_ok(),
        };
        let why_not = |e: Self| match e {
            Self::Rift => loaded.mock.as_ref().err().cloned(),
            Self::Http => loaded.proxy.as_ref().err().cloned(),
        };
        let wanted = match Self::parse(requested)? {
            Some(wanted) => wanted,
            None if exports(Self::Rift) => return Ok(Self::Rift),
            None => return Ok(Self::Http),
        };
        if exports(wanted) {
            return Ok(wanted);
        }
        let other = match wanted {
            Self::Rift => Self::Http,
            Self::Http => Self::Rift,
        };
        Err(WasmAdmitError::World(format!(
            "export \"{}\" needs {}, which this component does not export (it exports {}): {}",
            wanted.as_str(),
            wanted.interface(),
            other.interface(),
            why_not(wanted).as_deref().unwrap_or("not exported")
        )))
    }
}

static CACHE: LazyLock<parking_lot::Mutex<HashMap<u64, Arc<Loaded>>>> =
    LazyLock::new(Default::default);
/// Compiles per component, by its code, for the cache tests (others compile concurrently, so a
/// process-wide count cannot be exact).
#[cfg(test)]
static COMPILES: LazyLock<parking_lot::Mutex<HashMap<String, u64>>> =
    LazyLock::new(Default::default);

/// How many times `code` has been compiled in this process.
#[cfg(test)]
pub(crate) fn compile_count(code: &str) -> u64 {
    COMPILES.lock().get(code).copied().unwrap_or(0)
}

/// A cheap key over a component's base64 text: its length and its first and last 4 KiB. Hashing
/// all of a ~200 KiB text would cost more than running the component; a hit compares it whole.
fn fingerprint(code: &str) -> u64 {
    const EDGE: usize = 4096;
    let bytes = code.as_bytes();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.len().hash(&mut h);
    bytes[..bytes.len().min(EDGE)].hash(&mut h);
    bytes[bytes.len().saturating_sub(EDGE)..].hash(&mut h);
    h.finish()
}

fn cached(code: &str) -> Option<Arc<Loaded>> {
    let key = fingerprint(code);
    // Compared outside the lock: the texts are long, and every wasm request takes this lock.
    let entry = CACHE.lock().get(&key).cloned()?;
    (*entry.code == *code).then_some(entry)
}

/// The interfaces a component may import, by name prefix. Everything else is refused at admission
/// by name, before linking: what a component imports is read from the component itself, so the
/// refusal does not depend on how wasmtime words a link failure.
const ALLOWED_IMPORTS: &[&str] = &[
    "wasi:cli/",
    "wasi:io/",
    "wasi:filesystem/",
    "wasi:clocks/",
    "wasi:random/",
    "rift:mock/state@",
    "rift:mock/types@",
    // Issue #1758: the types an http component is built from; never `wasi:http/outgoing-handler`.
    "wasi:http/types@",
];

/// The component in `code`, compiled and linked, from the cache or compiled now.
fn load(code: &str) -> Result<Arc<Loaded>, WasmAdmitError> {
    if let Some(hit) = cached(code) {
        return Ok(hit);
    }
    let loaded = Arc::new(compile(code)?);
    let key = fingerprint(code);
    let mut cache = CACHE.lock();
    if !cache.contains_key(&key) && cache.len() >= CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(key, Arc::clone(&loaded));
    Ok(loaded)
}

/// Decode, compile, check the imports of and link the component in `code`, bypassing the cache.
fn compile(code: &str) -> Result<Loaded, WasmAdmitError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(code.trim())
        .map_err(|e| WasmAdmitError::NotBase64(e.to_string()))?;
    let engine = engine().map_err(|e| WasmAdmitError::Unavailable(format!("{e:#}")))?;
    let linker = linker().map_err(|e| WasmAdmitError::Unavailable(format!("{e:#}")))?;
    #[cfg(test)]
    {
        *COMPILES.lock().entry(code.to_owned()).or_default() += 1;
    }
    let component =
        Component::new(engine, &bytes).map_err(|e| WasmAdmitError::NotComponent(root(&e)))?;
    let imports: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name.to_owned())
        .collect();
    if let Some(name) = imports
        .iter()
        .find(|name| !ALLOWED_IMPORTS.iter().any(|p| name.starts_with(p)))
    {
        return Err(WasmAdmitError::Import(name.clone()));
    }
    let pre = linker
        .instantiate_pre(&component)
        .map_err(|e| WasmAdmitError::World(root(&e)))?;
    let mock = MockPre::new(pre.clone()).map_err(|e| Arc::from(root(&e)));
    let proxy = ProxyPre::new(pre).map_err(|e| Arc::from(root(&e)));
    if let (Err(mock), Err(proxy)) = (&mock, &proxy) {
        return Err(WasmAdmitError::World(format!(
            "it exports neither rift:mock/handler ({mock}) nor wasi:http/incoming-handler ({proxy})"
        )));
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let sha12: String = digest.as_ref()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Loaded {
        code: Arc::from(code),
        sha12: Arc::from(sha12),
        imports: Arc::from(imports),
        mock,
        proxy,
    })
}

fn root(e: &wasmtime::Error) -> String {
    e.root_cause().to_string()
}

/// Admission (issue #1751): `code` is a component Rift can run under `export`. Compiling it here
/// warms the cache for the first request.
///
/// Validation is synchronous and runs on the admin request's task, and a compile can take seconds
/// for a large component; on a multi-threaded runtime that task's worker hands its other tasks on
/// while this compiles.
pub(crate) fn admit(code: &str, export: Option<&str>) -> Result<(), WasmAdmitError> {
    admit_export(code, export).map(drop)
}

/// As [`admit`], returning the export that answers (`"rift"` or `"http"`, issue #1758).
pub(crate) fn admit_export(
    code: &str,
    export: Option<&str>,
) -> Result<&'static str, WasmAdmitError> {
    WasmExport::parse(export)?;
    let loaded = admitted(code)?;
    WasmExport::resolve(export, &loaded).map(WasmExport::as_str)
}

/// As [`admit`] for the default export, returning what the component imports (issue #1757). Every
/// component that compiles has a default export: `compile` refuses one with neither.
pub(crate) fn admit_imports(code: &str) -> Result<Vec<String>, WasmAdmitError> {
    admitted(code).map(|loaded| loaded.imports.to_vec())
}

fn admitted(code: &str) -> Result<Arc<Loaded>, WasmAdmitError> {
    let on_worker = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if on_worker {
        tokio::task::block_in_place(|| load(code))
    } else {
        load(code)
    }
}

/// How a call failed, as `rift_wasm_traps_total` labels it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrapKind {
    /// The `timeoutMs` deadline.
    Deadline,
    /// The `fuel` budget.
    Fuel,
    /// `maxMemoryMiB`.
    Memory,
    /// Any other trap.
    Trap,
    /// `init` or `respond` returned `err`; an http component set an error, set no response, or
    /// its body failed.
    GuestError,
}

impl TrapKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Fuel => "fuel",
            Self::Memory => "memory",
            Self::Trap => "trap",
            Self::GuestError => "guest-error",
        }
    }
}

/// What one call produced besides its decision: for the debug trace and the log.
#[derive(Debug, Default)]
pub(crate) struct CallReport {
    pub(crate) fuel_used: Option<u64>,
    /// How the call trapped, and the detail.
    pub(crate) trap: Option<(TrapKind, String)>,
    /// What the call wrote to stdout and stderr, for [`emit_logs`].
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// The component's short SHA-256, which tags its log lines.
    sha12: Option<Arc<str>>,
}

impl CallReport {
    /// The trap as the debug trace shows it: `fuel`, or `memory: <detail>`.
    pub(crate) fn trap_text(&self) -> Option<String> {
        self.trap.as_ref().map(|(kind, detail)| {
            if detail.is_empty() {
                kind.as_str().to_owned()
            } else {
                format!("{}: {detail}", kind.as_str())
            }
        })
    }

    fn trapped(&mut self, kind: TrapKind, detail: impl Into<String>) {
        crate::extensions::metrics::record_wasm_trap(kind.as_str());
        self.trap = Some((kind, detail.into()));
    }
}

/// Run the component once: a fresh store, then `init(knobs)` and `respond(request)` (`export:
/// "rift"`) or `wasi:http/incoming-handler.handle` (`export: "http"`), then the store is dropped.
/// The epoch deadline is `timeout` from now; `timeout` also bounds the whole call from outside.
pub(crate) async fn respond(
    script: WasmScript<'_>,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
    ctx_extra: &ScriptCtxExtras,
) -> (anyhow::Result<WasmAnswer>, CallReport) {
    let mut report = CallReport::default();
    let result = run(
        script,
        rule_id,
        request,
        flow_store,
        timeout,
        ctx_extra,
        &mut report,
    )
    .await;
    (result, report)
}

async fn run(
    script: WasmScript<'_>,
    rule_id: &str,
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    timeout: Duration,
    ctx_extra: &ScriptCtxExtras,
    report: &mut CallReport,
) -> anyhow::Result<WasmAnswer> {
    WasmExport::parse(script.export)?;
    let started = Instant::now();
    let loaded = match cached(script.code) {
        Some(hit) => hit,
        // A miss is a component admitted before a restart or a replay: compile it off the request
        // task, as it takes tens of milliseconds, and within the call's deadline.
        None => {
            let code = script.code.to_owned();
            let compile = tokio::task::spawn_blocking(move || load(&code));
            match tokio::time::timeout(timeout, compile).await {
                Ok(joined) => joined.map_err(|e| anyhow!("wasm compile task failed: {e}"))??,
                Err(_elapsed) => {
                    report.trapped(TrapKind::Deadline, "compiling");
                    return Err(timeout_error(timeout));
                }
            }
        }
    };
    let export = WasmExport::resolve(script.export, &loaded)?;
    // What is left of the call's deadline after any compile.
    let remaining = timeout.saturating_sub(started.elapsed());
    report.sha12 = Some(Arc::clone(&loaded.sha12));
    let ctx = ctx_extra.build_ctx_input(request);
    let knobs = script
        .knobs
        .map_or_else(|| "{}".to_owned(), serde_json::Value::to_string);
    let limits = script.limits;

    let stdout = LogPipe::new(LOG_PIPE_BYTES);
    let stderr = LogPipe::new(LOG_PIPE_BYTES);
    // `wasi:http` has no `init`: an http component reads its knobs from `RIFT_KNOBS`, the one
    // variable any component ever sees.
    let knobs_env = (export == WasmExport::Http).then_some(knobs.as_str());
    let (mut store, budget) = new_store(
        &limits,
        script.clock,
        flow_store,
        ctx.flow_id.clone(),
        (stdout.clone(), stderr.clone()),
        knobs_env,
        remaining,
    )?;
    // The epoch deadline normally ends a runaway call first, as a trap the store reports; this
    // outer bound, two ticks later, catches a call parked in a host import.
    let bound = remaining.saturating_add(2 * EPOCH_TICK);
    let failed = Failure {
        budget,
        max_memory_mib: limits.max_memory_mib,
        timeout,
    };

    match export {
        WasmExport::Rift => {
            let pre = loaded.mock.as_ref().map_err(|e| anyhow!("{e}"))?;
            let wit_request = to_wit_request(request, &ctx)?;
            let call = async {
                let instance = pre.instantiate_async(&mut store).await?;
                let handler = instance.rift_mock_handler();
                if let Err(msg) = handler.call_init(&mut store, &knobs).await? {
                    return Ok(Err(format!("init: {msg}")));
                }
                handler.call_respond(&mut store, &wit_request).await
            };
            let outcome = tokio::time::timeout(bound, call).await;
            let memory_refused = settle(store, budget, (&stdout, &stderr), report);
            match failed.classify(outcome, report, memory_refused)? {
                Ok(decision) => decision_to_fault(decision, rule_id).map(WasmAnswer::Decision),
                Err(guest) => {
                    report.trapped(TrapKind::GuestError, &guest);
                    Err(anyhow!("{guest}"))
                }
            }
        }
        WasmExport::Http => {
            let pre = loaded.proxy.as_ref().map_err(|e| anyhow!("{e}"))?;
            let http_request = to_http_request(request, ctx_extra)?;
            let body_limit =
                usize::try_from(u64::from(limits.max_memory_mib) << 20).unwrap_or(usize::MAX);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let collect = collect_response(receiver, body_limit);
            tokio::pin!(collect);
            let mut collected = None;
            let call = async {
                let proxy = pre.instantiate_async(&mut store).await?;
                let scheme = if ctx_extra.https {
                    Scheme::Https
                } else {
                    Scheme::Http
                };
                let req = store
                    .data_mut()
                    .http()
                    .new_incoming_request(scheme, http_request)?;
                let out = store.data_mut().http().new_response_outparam(sender)?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, req, out)
                    .await
            };
            // The body is read while `handle` runs: a guest can set the response and keep writing
            // the body inside the call, and a full body buffer waits for this side to read.
            let outcome = tokio::time::timeout(bound, async {
                tokio::pin!(call);
                loop {
                    tokio::select! {
                        out = &mut call => break out,
                        got = &mut collect, if collected.is_none() => collected = Some(got),
                    }
                }
            })
            .await;
            // Dropping the store closes the outparam and the body, so what is left to read ends.
            let memory_refused = settle(store, budget, (&stdout, &stderr), report);
            // A body past the bound stops this side reading, and a guest whose next write then
            // fails usually traps: the bound is the cause to report, not the trap it led to.
            if !matches!(collected, Some(Collected::TooLarge)) {
                failed.classify(outcome, report, memory_refused)?;
            }
            let collected = match collected {
                Some(collected) => collected,
                None => collect.await,
            };
            match collected {
                Collected::Response(response) => Ok(WasmAnswer::Response(response)),
                Collected::TooLarge => {
                    let msg = format!(
                        "response body exceeds maxMemoryMiB ({} MiB)",
                        limits.max_memory_mib
                    );
                    report.trapped(TrapKind::Memory, &msg);
                    Err(anyhow!("{msg}"))
                }
                Collected::Failed(msg) => {
                    report.trapped(TrapKind::GuestError, &msg);
                    Err(anyhow!("{msg}"))
                }
            }
        }
    }
}

/// What an http component's response came to.
enum Collected {
    Response(WasmResponse),
    /// The body passed the bound.
    TooLarge,
    /// No response, an error response, or a body that failed.
    Failed(String),
}

/// The response an http component sets on its outparam, with its whole body, at most `limit` bytes.
async fn collect_response(
    receiver: tokio::sync::oneshot::Receiver<Result<hyper::Response<HyperOutgoingBody>, ErrorCode>>,
    limit: usize,
) -> Collected {
    use http_body_util::BodyExt as _;
    let response = match receiver.await {
        Ok(Ok(response)) => response,
        Ok(Err(code)) => {
            return Collected::Failed(format!("component answered an error: {code:?}"));
        }
        // The outparam was dropped unset.
        Err(_) => return Collected::Failed("component returned no response".to_owned()),
    };
    let (parts, body) = response.into_parts();
    match http_body_util::Limited::new(body, limit).collect().await {
        Ok(body) => {
            let body = body.to_bytes();
            // A guest that returns holding an unfinished body ends it cleanly when the store goes:
            // the length it declared is what tells a cut-off body from a whole one.
            if let Some(declared) = parts.headers.get(hyper::header::CONTENT_LENGTH) {
                let parsed = declared
                    .to_str()
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok());
                if parsed != Some(body.len()) {
                    return Collected::Failed(format!(
                        "response body is {} bytes, but its content-length is {declared:?}",
                        body.len()
                    ));
                }
            }
            Collected::Response(WasmResponse {
                status: parts.status.as_u16(),
                headers: parts.headers,
                body,
            })
        }
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Collected::TooLarge
        }
        Err(e) => Collected::Failed(format!("response body failed: {e}")),
    }
}

/// The request an http component handles: method, path and query, every header value, the body's
/// bytes. The query is the one the client sent when the caller has it (`ScriptCtxExtras::raw_query`),
/// else it is encoded again from its parsed pairs. A request without a `Host` header (an HTTP/2
/// one, say) gets the imposter's address, since `wasi:http` requires an authority.
fn to_http_request(
    request: &ScriptRequest,
    extras: &ScriptCtxExtras,
) -> anyhow::Result<hyper::Request<http_body_util::Full<bytes::Bytes>>> {
    let port = extras.port;
    let mut target = request.path.clone();
    match extras.raw_query.as_deref() {
        Some("") => {}
        Some(raw) => {
            target.push('?');
            target.push_str(raw);
        }
        None => {
            let pairs = query_pairs(&request.query);
            if !pairs.is_empty() {
                target.push('?');
                target.push_str(
                    &url::form_urlencoded::Serializer::new(String::new())
                        .extend_pairs(pairs)
                        .finish(),
                );
            }
        }
    }
    let method = hyper::Method::from_bytes(request.method.as_bytes())
        .map_err(|e| anyhow!("request method {:?}: {e}", request.method))?;
    let mut builder = hyper::Request::builder().method(method).uri(target);
    let mut has_host = false;
    for (name, value) in query_pairs(&request.headers) {
        has_host |= name.eq_ignore_ascii_case("host");
        builder = builder.header(name, value);
    }
    if !has_host {
        builder = builder.header("host", format!("127.0.0.1:{port}"));
    }
    builder
        .body(http_body_util::Full::new(bytes::Bytes::from(request_body(
            request,
        )?)))
        .map_err(|e| anyhow!("request for the http export: {e}"))
}

/// Every value of `map`, one pair each, in the map's order.
fn query_pairs(map: &crate::util::MultiValueMap) -> Vec<(String, String)> {
    map.iter()
        .flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone())))
        .collect()
}

/// The request body's bytes: the raw text, or a binary body's base64 decoded.
fn request_body(request: &ScriptRequest) -> anyhow::Result<Vec<u8>> {
    let raw = request
        .raw_body
        .clone()
        .unwrap_or_else(|| match &request.body {
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        });
    match request.mode {
        ResponseMode::Text => Ok(raw.into_bytes()),
        ResponseMode::Binary => base64::engine::general_purpose::STANDARD
            .decode(raw.as_bytes())
            .map_err(|e| anyhow!("binary request body is not base64: {e}")),
    }
}

/// After a call: the fuel it spent and what it logged, into `report`; then the store is dropped.
/// Returns whether the memory cap refused a growth.
fn settle(
    store: Store<Host>,
    budget: u64,
    logs: (&LogPipe, &LogPipe),
    report: &mut CallReport,
) -> bool {
    // Fuel is always metered (`consume_fuel` is on), so this only fails on a misconfigured engine;
    // the trace then omits `fuelUsed`.
    report.fuel_used = store
        .get_fuel()
        .ok()
        .map(|left| budget.saturating_sub(left));
    let memory_refused = store.data().limits.memory_refused;
    drop(store);
    report.stdout = logs.0.contents();
    report.stderr = logs.1.contents();
    memory_refused
}

/// A store for one call: WASI with nothing behind it but the captured `logs` (stdout, stderr) and
/// `clock`, the `rift:mock/state` host on `flow_store`, `limits`' memory, table and fuel caps, and
/// an epoch deadline `remaining` from now. Returns the store and its fuel budget.
fn new_store(
    limits: &WasmLimits,
    clock: Option<&ScriptClock>,
    flow_store: Arc<dyn FlowStore>,
    flow_id: String,
    logs: (LogPipe, LogPipe),
    knobs_env: Option<&str>,
    remaining: Duration,
) -> anyhow::Result<(Store<Host>, u64)> {
    let memory_size = usize::try_from(u64::from(limits.max_memory_mib) << 20).map_err(|_| {
        anyhow!(
            "maxMemoryMiB {} is more than this host can address",
            limits.max_memory_mib
        )
    })?;
    let mut wasi = WasiCtxBuilder::new();
    wasi.stdout(logs.0).stderr(logs.1);
    if let Some(knobs) = knobs_env {
        wasi.env("RIFT_KNOBS", knobs);
    }
    // `wasi:random` draws from the request's stream (issue #1755), so a seeded imposter replays.
    // "Secure" random is deterministic under a seed too: this is a mock.
    if let Some(mut rng) = crate::imposter::rng::current() {
        wasi.insecure_random_seed(rng.seed_u128())
            .secure_random(rng.clone())
            .insecure_random(rng);
    }
    if let Some(clock) = clock {
        wasi.wall_clock(GuestWallClock(clock.clone()))
            .monotonic_clock(GuestMonotonicClock(clock.clone()));
    }
    let mut table = ResourceTable::new();
    table.set_max_capacity(MAX_RESOURCES);
    let host = Host {
        wasi: wasi.build(),
        table,
        http: {
            let mut http = WasiHttpCtx::new();
            http.set_field_size_limit(FIELD_SIZE_BYTES);
            http
        },
        http_hooks: NoOutgoing,
        limits: Limits {
            inner: StoreLimitsBuilder::new()
                .instances(MAX_INSTANCES)
                .memories(MAX_INSTANCES)
                .tables(limits.max_tables as usize)
                .table_elements(limits.max_table_elements as usize)
                .trap_on_grow_failure(true)
                .build(),
            memory_cap: memory_size,
            memory_total: 0,
            memory_refused: false,
        },
        flow_store,
        flow_id,
    };
    let mut store = Store::new(engine()?, host);
    store.limiter(|h| &mut h.limits);
    let budget = limits.fuel.unwrap_or(u64::MAX);
    store.set_fuel(budget).map_err(|e| anyhow!("{e:#}"))?;
    // A deadline past what `Instant` can hold is no deadline: the outer timeout still bounds the
    // call, as it bounds a Rhai or JavaScript script.
    let deadline = Instant::now().checked_add(remaining);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        Ok(match deadline {
            Some(at) if Instant::now() >= at => UpdateDeadline::Interrupt,
            _ => UpdateDeadline::Yield(1),
        })
    });
    Ok((store, budget))
}

/// What the script-engine benches measure apart (issue #1760): the parts of a call `respond` runs
/// together. Not an API.
#[doc(hidden)]
pub mod bench {
    use super::{
        Arc, Duration, FlowStore, LOG_PIPE_BYTES, LogPipe, WasmLimits, anyhow, compile, load,
        new_store,
    };

    /// Compile and link `code` (base64) without the cache: what admitting a component costs.
    pub fn compile_uncached(code: &str) -> anyhow::Result<()> {
        compile(code).map(drop).map_err(|e| anyhow!("{e}"))
    }

    /// A fresh store, the component instantiated in it and `init("{}")` run: what every call pays
    /// before `respond`. `code` is compiled on first use and cached.
    pub async fn instantiate(code: &str, flow_store: Arc<dyn FlowStore>) -> anyhow::Result<()> {
        let loaded = load(code).map_err(|e| anyhow!("{e}"))?;
        let logs = (LogPipe::new(LOG_PIPE_BYTES), LogPipe::new(LOG_PIPE_BYTES));
        let (mut store, _) = new_store(
            &WasmLimits::default(),
            None,
            flow_store,
            String::new(),
            logs,
            None,
            Duration::from_secs(5),
        )?;
        let pre = loaded
            .mock
            .as_ref()
            .map_err(|e| anyhow!("the bench component has no rift export: {e}"))?;
        let instance = pre.instantiate_async(&mut store).await?;
        instance
            .rift_mock_handler()
            .call_init(&mut store, "{}")
            .await?
            .map_err(|msg| anyhow!("init: {msg}"))
    }
}

/// A deadline miss, as the `_rift.script` path reports one for every engine.
fn timeout_error(timeout: Duration) -> anyhow::Error {
    ScriptTimeoutError {
        hook: "_rift.script",
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    }
    .into()
}

/// What a call's trap is reported against.
struct Failure {
    budget: u64,
    max_memory_mib: u32,
    timeout: Duration,
}

impl Failure {
    /// A call's outcome as its value, or as a script error recording how it trapped.
    fn classify<T>(
        &self,
        outcome: Result<wasmtime::Result<T>, tokio::time::error::Elapsed>,
        report: &mut CallReport,
        memory_refused: bool,
    ) -> anyhow::Result<T> {
        let e = match outcome {
            Err(_elapsed) => {
                report.trapped(TrapKind::Deadline, "");
                return Err(timeout_error(self.timeout));
            }
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) => e,
        };
        match e.downcast_ref::<Trap>() {
            Some(Trap::Interrupt) => {
                report.trapped(TrapKind::Deadline, "");
                Err(timeout_error(self.timeout))
            }
            Some(Trap::OutOfFuel) => {
                report.trapped(TrapKind::Fuel, "");
                Err(anyhow!("fuel exhausted ({} units)", self.budget))
            }
            _ if memory_refused => {
                let cause = root(&e);
                report.trapped(TrapKind::Memory, &cause);
                Err(anyhow!(
                    "memory limit exceeded (maxMemoryMiB {}): {cause}",
                    self.max_memory_mib
                ))
            }
            _ => {
                let cause = root(&e);
                report.trapped(TrapKind::Trap, &cause);
                Err(anyhow!("wasm trap: {cause}"))
            }
        }
    }
}

/// Each line a call wrote, as the `ctx.logger` event Rhai and JavaScript emit: stdout at info,
/// stderr at warn, tagged with the component's short SHA-256.
pub(crate) fn emit_logs(report: &CallReport) {
    let sha = report.sha12.as_deref().unwrap_or("");
    for line in String::from_utf8_lossy(&report.stdout).lines() {
        tracing::info!(target: crate::scripting::trace::SCRIPT_LOG_TARGET, engine = "wasm", sha, "{line}");
    }
    for line in String::from_utf8_lossy(&report.stderr).lines() {
        tracing::warn!(target: crate::scripting::trace::SCRIPT_LOG_TARGET, engine = "wasm", sha, "{line}");
    }
}

fn to_wit_request(
    request: &ScriptRequest,
    ctx: &crate::scripting::ScriptCtxInput<'_>,
) -> anyhow::Result<Request> {
    let pairs = query_pairs;
    let body = request_body(request)?;
    let mut path_params: Vec<(String, String)> = request
        .path_params
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    path_params.sort();
    Ok(Request {
        method: request.method.clone(),
        path: request.path.clone(),
        query: pairs(&request.query),
        path_params,
        headers: pairs(&request.headers)
            .into_iter()
            .map(|(name, value)| Header { name, value })
            .collect(),
        body,
        flow_id: ctx.flow_id.clone(),
        port: ctx.port,
        stub_id: ctx.stub.stub_id.clone(),
        scenario_name: ctx.stub.scenario_name.clone(),
        scenario_state: ctx.stub.scenario_state.clone(),
    })
}

fn decision_to_fault(decision: Decision, rule_id: &str) -> anyhow::Result<FaultDecision> {
    let result = match decision {
        Decision::Pass => ScriptResult::Pass,
        Decision::Delay(ms) => ScriptResult::Delay(ms),
        Decision::Reset => ScriptResult::Reset,
        Decision::Respond(response) => {
            let body = String::from_utf8(response.body).map_err(|_| {
                anyhow!("response body is not UTF-8; use export \"http\" for binary bodies")
            })?;
            ScriptResult::Http {
                status: response.status,
                body: Some(ScriptResultBody::Str(body)),
                headers: response
                    .headers
                    .into_iter()
                    .map(|h| (h.name, h.value))
                    .collect(),
            }
        }
    };
    Ok(result.into_fault_decision(rule_id))
}

/// A component's `wasi:clocks/wall-clock` (issue #1754): the imposter's frozen instant, a request's
/// `X-Rift-Now`, or the system clock. Read at every call, so a step taken mid-call is seen.
struct GuestWallClock(crate::scripting::ScriptClock);

impl wasmtime_wasi::HostWallClock for GuestWallClock {
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn now(&self) -> Duration {
        match self.0.frozen_ms() {
            // An instant before the epoch reads as the epoch: WASI's wall clock is unsigned.
            Some(ms) => Duration::from_millis(u64::try_from(ms).unwrap_or(0)),
            None => std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO),
        }
    }
}

/// When the process's monotonic clock started: shared by every store, so a component's monotonic
/// reads do not go back between calls, as each call gets a fresh store.
static MONOTONIC_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// A component's `wasi:clocks/monotonic-clock` (issue #1754): under a frozen clock its nanoseconds
/// are the frozen instant's, so two reads of one frozen instant are equal; otherwise time since the
/// process started. Never decreasing while the mode stays the same.
struct GuestMonotonicClock(crate::scripting::ScriptClock);

impl wasmtime_wasi::HostMonotonicClock for GuestMonotonicClock {
    fn resolution(&self) -> u64 {
        1
    }

    fn now(&self) -> u64 {
        match self.0.frozen_ms() {
            Some(ms) => u64::try_from(ms).unwrap_or(0).saturating_mul(1_000_000),
            None => u64::try_from(MONOTONIC_ORIGIN.elapsed().as_nanos()).unwrap_or(u64::MAX),
        }
    }
}
