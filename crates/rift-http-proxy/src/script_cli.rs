//! `rift script check` / `rift script run` (issue #360): scripting DX tools that validate or
//! execute a script without a running server. Kept as plain, testable library functions —
//! `main.rs`/[`dispatch`] are thin CLI wrappers (arg parsing, printing, exit codes) around
//! [`run_check`] and [`run_run`], which tests call directly.

use crate::config_loader::{self, ConfigSource};
use crate::flow_state::FlowStore;
use crate::imposter::{RiftScriptConfig, StubResponse};
use crate::server::ScriptAction;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// ===========================================================================
// `rift script check`
// ===========================================================================

/// The result of `rift script check <target>`: zero or more errors/warnings, each already
/// formatted with its location (file, or `imposter[i].stubs[j].responses[k]` for a config).
#[derive(Debug, Default)]
pub struct CheckReport {
    pub target: String,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl CheckReport {
    fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Statically validate `target` — a raw script file (`.rhai`/`.js`) or a rift config file
/// (`.json`/`.yaml`/`.yml`) — with no server running. `hook` is only used for a raw script file
/// (a config's `_rift.script` entries are always response-position, i.e. `respond`).
pub fn run_check(target: &Path, hook: &str) -> Result<CheckReport> {
    if !target.exists() {
        bail!("file not found: {}", target.display());
    }
    match target_kind(target)? {
        TargetKind::Script(engine) => check_raw_script(target, &engine, hook),
        TargetKind::Config => check_config(target),
    }
}

enum TargetKind {
    Script(String),
    Config,
}

fn target_kind(path: &Path) -> Result<TargetKind> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rhai") => Ok(TargetKind::Script("rhai".to_string())),
        Some("js") => Ok(TargetKind::Script("javascript".to_string())),
        Some("lua") => Err(anyhow!(
            "the Lua scripting engine was removed (issue #450); rewrite {} as a .rhai or .js script",
            path.display()
        )),
        Some("json") | Some("yaml") | Some("yml") => Ok(TargetKind::Config),
        other => Err(anyhow!(
            "cannot determine target kind from extension {:?} of {}; expected .rhai/.js \
             (script) or .json/.yaml/.yml (config)",
            other,
            path.display()
        )),
    }
}

fn check_raw_script(path: &Path, engine: &str, hook: &str) -> Result<CheckReport> {
    let code = std::fs::read_to_string(path)
        .with_context(|| format!("reading script file {}", path.display()))?;
    let mut report = CheckReport::new(path.display().to_string());
    check_one_script(&code, engine, hook, path.display().to_string(), &mut report);
    Ok(report)
}

/// Run every static check this module knows over one script, appending findings to `report`
/// (each message already prefixed with `location`).
fn check_one_script(
    code: &str,
    engine: &str,
    hook: &str,
    location: String,
    report: &mut CheckReport,
) {
    // The key new check (issue #360): entrypoint presence/arity for the intended hook. Reuses
    // the same declared-function detection issue #357 added to each engine's runtime dispatch
    // (`run_entrypoint`/`should_inject_with_ctx`) — see `scripting::entrypoint_check` — so a
    // script that only compiles/parses but calls the wrong function name is caught here instead
    // of at request time.
    if let Err(e) = crate::scripting::check_entrypoint(engine, code, hook) {
        report.errors.push(format!("{location}: {e}"));
    }
}

fn check_config(path: &Path) -> Result<CheckReport> {
    let mut report = CheckReport::new(path.display().to_string());

    // Reuses the exact `--configfile` load path (issue #356 `file:`/`ref:` resolution, EJS
    // preprocessing, single/array/`{"imposters":[...]}` shape handling) — a config `script
    // check` sees precisely what a real `rift --configfile` startup would.
    let configs = config_loader::load_configs(&ConfigSource::File {
        path: path.to_path_buf(),
        no_parse: false,
    })
    .with_context(|| format!("loading config {}", path.display()))?;

    for (cfg_idx, config) in configs.iter().enumerate() {
        let has_flow_state = config
            .rift
            .as_ref()
            .and_then(|r| r.flow_state.as_ref())
            .is_some();

        for (stub_idx, stub) in config.stubs.iter().enumerate() {
            for (resp_idx, response) in stub.responses.iter().enumerate() {
                let Some(script_cfg) = response_script(response) else {
                    continue;
                };
                let location = if configs.len() == 1 {
                    format!("stubs[{stub_idx}].responses[{resp_idx}]")
                } else {
                    format!("imposter[{cfg_idx}].stubs[{stub_idx}].responses[{resp_idx}]")
                };
                check_config_script(script_cfg, has_flow_state, location, &mut report);
            }
        }
    }
    Ok(report)
}

/// One resolved `_rift.script` config: syntax + entrypoint (always `respond` — every
/// `_rift.script` here is response-position), and state-without-flowState (rift-lint E042's
/// check, re-implemented here against the typed config rather than the raw JSON `Value`
/// rift-lint walks — see the module docs on why this crate doesn't pull in rift-lint itself).
fn check_config_script(
    script_cfg: &RiftScriptConfig,
    has_flow_state: bool,
    location: String,
    report: &mut CheckReport,
) {
    let Some(code) = script_cfg.code.as_deref() else {
        // `config_loader::load_configs` always resolves file:/ref: into `code` before returning
        // — a `None` here would mean that pass was skipped, which would already have errored.
        report.errors.push(format!(
            "{location}: script has no resolved source (internal error)"
        ));
        return;
    };
    let engine = script_cfg.engine.as_deref().unwrap_or("rhai");
    check_one_script(code, engine, "respond", location.clone(), report);

    if !has_flow_state && (code.contains("ctx.state") || code.contains("flow_store")) {
        report.warnings.push(format!(
            "{location}: uses ctx.state (or flow_store) but no _rift.flowState is configured; \
             state will be auto-provisioned in-memory (won't persist across restarts or be \
             shared across a cluster) — configure _rift.flowState for production"
        ));
    }
}

/// Borrow the `_rift.script` config out of a stub response, if it has one — covers both the
/// `is` response's optional `_rift` extension and the script-only `RiftScript` response (mirrors
/// `imposter::script_resolve`'s private `response_script_mut`, read-only).
fn response_script(response: &StubResponse) -> Option<&RiftScriptConfig> {
    match response {
        StubResponse::Is {
            rift: Some(rift), ..
        } => rift.script.as_ref(),
        StubResponse::RiftScript { rift } => rift.script.as_ref(),
        _ => None,
    }
}

// ===========================================================================
// `rift script run`
// ===========================================================================

/// The result of `rift script run <target>`.
#[derive(Debug)]
pub struct RunReport {
    /// The rendered decision (`pass()` / `http(503) ...` / `delay(42ms)` / `reset()`), or
    /// `"error"` when the script itself failed — see `error` for the message in that case.
    pub decision: String,
    pub duration_ms: u64,
    pub logs: Vec<String>,
    /// The flow's state after the run, sorted by key.
    pub state: Vec<(String, serde_json::Value)>,
    pub error: Option<String>,
}

/// The request-object shape scripts see (issue #360 Item 2's `--request` fixture): the same
/// fields `ScriptRequest` carries, all optional so a minimal fixture (or none at all) is valid.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RequestFixture {
    method: Option<String>,
    path: Option<String>,
    headers: HashMap<String, String>,
    query: HashMap<String, String>,
    path_params: HashMap<String, String>,
    body: serde_json::Value,
}

fn fixture_to_script_request(fixture: RequestFixture) -> crate::scripting::ScriptRequest {
    let raw_body = if fixture.body.is_null() {
        None
    } else {
        Some(fixture.body.to_string())
    };
    crate::scripting::ScriptRequest {
        method: fixture.method.unwrap_or_else(|| "GET".to_string()),
        path: fixture.path.unwrap_or_else(|| "/".to_string()),
        headers: fixture.headers,
        query: fixture.query,
        path_params: fixture.path_params,
        body: fixture.body,
        raw_body,
        // `--request` fixtures are authored as JSON, so a binary body can't be expressed here
        // (issue #636 covers the live serve/proxy paths, not this offline debugging fixture).
        mode: crate::imposter::ResponseMode::Text,
    }
}

/// Infer the script engine from `path`'s extension (`.rhai`/`.js`); `None` when it isn't
/// one of the two recognized extensions.
fn infer_engine_from_extension(path: &Path) -> Option<String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rhai") => Some("rhai".to_string()),
        Some("js") => Some("javascript".to_string()),
        _ => None,
    }
}

/// Parse a `--state key=value` entry: the value is JSON if it parses, else a plain string (issue
/// #360 Item 2).
fn parse_state_entry(entry: &str) -> Result<(String, serde_json::Value)> {
    let (key, raw_value) = entry
        .split_once('=')
        .ok_or_else(|| anyhow!("--state must be `key=value`, got '{entry}'"))?;
    let value = serde_json::from_str(raw_value)
        .unwrap_or_else(|_| serde_json::Value::String(raw_value.to_string()));
    Ok((key.to_string(), value))
}

/// Execute `target` against a fixture request and seeded flow state, with no server running
/// (issue #360 Item 2). Only `hook == "respond"` is supported today — `matches`/`transform`/
/// `delay` are Rhai-only and not wired end-to-end outside the engine's own unit tests (see
/// `ScriptEngine::should_inject_fault_with_ctx`, the only engine-agnostic entrypoint that
/// exists); asking for another hook is a clean error, not a silent no-op.
#[allow(clippy::too_many_arguments)]
pub fn run_run(
    target: &Path,
    request_path: Option<&Path>,
    state_entries: &[String],
    flow_id: &str,
    engine_override: Option<&str>,
    hook: &str,
) -> Result<RunReport> {
    if hook != "respond" {
        bail!(
            "`rift script run --hook {hook}` is not supported: only `respond` is wired \
             end-to-end across both engines today"
        );
    }

    let code = std::fs::read_to_string(target)
        .with_context(|| format!("reading script file {}", target.display()))?;
    let engine_type = match engine_override {
        Some(e) => e.to_string(),
        None => infer_engine_from_extension(target).ok_or_else(|| {
            anyhow!(
                "cannot infer script engine from extension of {}; pass --engine rhai|js",
                target.display()
            )
        })?,
    };

    let fixture: RequestFixture = match request_path {
        Some(p) => {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("reading request fixture {}", p.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing request fixture {}", p.display()))?
        }
        None => RequestFixture::default(),
    };
    let script_request = fixture_to_script_request(fixture);

    // A fresh, disposable in-memory store per run — never persisted, exactly like a real
    // request's auto-provisioned flow store when no `_rift.flowState` backend is configured. A
    // generous TTL keeps this run's seeded state alive for the run itself; nothing outlives the
    // process either way.
    let store = Arc::new(crate::backends::InMemoryFlowStore::new(3600));
    for entry in state_entries {
        let (key, value) = parse_state_entry(entry)?;
        store
            .set(flow_id, &key, value)
            .map_err(|e| anyhow!("seeding --state {entry}: {e}"))?;
    }
    let store_dyn: Arc<dyn FlowStore> = store.clone();

    // Surface a genuine compile/syntax error as a clean CLI error up front (rather than an
    // "error:" decision in the report).
    crate::scripting::ScriptEngine::new(&engine_type, &code, "cli-run")
        .with_context(|| format!("compiling {}", target.display()))?;
    let extras = crate::scripting::ScriptCtxExtras {
        flow_id: Some(flow_id.to_string()),
        ..Default::default()
    };

    // Run through the SAME bounded path the real proxy uses (issue #360): `spawn_blocking` +
    // wall-clock timeout with the Rhai abort flag wired in, so an infinite-loop script
    // TERMINATES the CLI at the deadline instead of hanging it. This also captures `ctx.logger`
    // output and the duration for free. A fresh current-thread runtime is built here because the
    // `script` subcommand runs before/without the server's tokio runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building CLI tokio runtime")?;
    let timeout = std::time::Duration::from_millis(crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS);
    let (result, entry) =
        runtime.block_on(crate::scripting::should_inject_bounded_with_ctx_traced(
            engine_type,
            code,
            "cli-run".to_string(),
            script_request,
            store_dyn,
            timeout,
            extras,
        ));

    let mut keys = store.keys_for_flow(flow_id);
    keys.sort();
    let state = keys
        .into_iter()
        .map(|k| {
            let v = store
                .get(flow_id, &k)
                .ok()
                .flatten()
                .unwrap_or(serde_json::Value::Null);
            (k, v)
        })
        .collect();

    // `entry` carries the rendered decision, duration, and (uncapped) logger lines; `result`
    // distinguishes a script error from a real decision for the exit code.
    match result {
        Ok(_) => Ok(RunReport {
            decision: entry.decision,
            duration_ms: entry.duration_ms,
            logs: entry.logs,
            state,
            error: None,
        }),
        Err(e) => Ok(RunReport {
            decision: "error".to_string(),
            duration_ms: entry.duration_ms,
            logs: entry.logs,
            state,
            error: Some(e.to_string()),
        }),
    }
}

// ===========================================================================
// CLI dispatch (printing + exit codes) — `main.rs` calls only this.
// ===========================================================================

/// Handle `rift script <check|run>`: run the library function, print a human-readable report,
/// and exit non-zero on failure. Returned `Err` is reserved for a usage/IO error (e.g. the
/// target file doesn't exist) that never got as far as producing a report.
pub fn dispatch(action: ScriptAction) -> Result<()> {
    match action {
        ScriptAction::Check { target, hook } => {
            let report = run_check(&target, &hook)?;
            print_check_report(&report);
            if report.is_ok() {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        ScriptAction::Run {
            target,
            request,
            state,
            flow_id,
            engine,
            hook,
        } => {
            let report = run_run(
                &target,
                request.as_deref(),
                &state,
                &flow_id,
                engine.as_deref(),
                &hook,
            )?;
            print_run_report(&report);
            if report.error.is_some() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn print_check_report(report: &CheckReport) {
    println!("Checking {}", report.target);
    for e in &report.errors {
        println!("  error: {e}");
    }
    for w in &report.warnings {
        println!("  warning: {w}");
    }
    if report.is_ok() {
        println!("OK");
    } else {
        println!(
            "FAILED ({} error(s), {} warning(s))",
            report.errors.len(),
            report.warnings.len()
        );
    }
}

fn print_run_report(report: &RunReport) {
    println!("decision: {}", report.decision);
    if let Some(err) = &report.error {
        println!("error: {err}");
    }
    println!("duration: {}ms", report.duration_ms);
    println!("state:");
    if report.state.is_empty() {
        println!("  (empty)");
    } else {
        for (k, v) in &report.state {
            println!("  {k} = {v}");
        }
    }
    println!("logs:");
    if report.logs.is_empty() {
        println!("  (none)");
    } else {
        for line in &report.logs {
            println!("  {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(dir: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(contents.as_bytes()).expect("write temp file");
        path
    }

    // ----- check: raw script -----

    // AC (issue #360): a valid v2 `respond` script passes.
    #[test]
    fn check_raw_v2_respond_script_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", "fn respond(ctx) { pass() }");
        let report = run_check(&path, "respond").expect("check runs");
        assert!(report.is_ok(), "expected OK, got {:?}", report.errors);
    }

    // Issue #453: v1 `should_inject` was removed — a script defining only `should_inject` is now
    // just a misnamed entrypoint, the same as any other wrong function name.
    #[test]
    fn check_raw_should_inject_only_script_fails_as_misnamed_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.rhai",
            "fn should_inject(request, flow_store) { #{ inject: false } }",
        );
        let report = run_check(&path, "respond").expect("check runs");
        assert!(
            !report.is_ok(),
            "should_inject-only script must fail as a misnamed entrypoint"
        );
        assert!(
            report.errors[0].contains("should_inject") && report.errors[0].contains("respond"),
            "error must name should_inject/respond, got {:?}",
            report.errors
        );
    }

    #[test]
    fn check_raw_js_should_inject_only_script_fails_as_misnamed_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.js",
            "function should_inject(request, flow_store) { return { inject: false }; }",
        );
        let report = run_check(&path, "respond").expect("check runs");
        assert!(
            !report.is_ok(),
            "should_inject-only JS script must fail as a misnamed entrypoint"
        );
        assert!(
            report.errors[0].contains("should_inject") && report.errors[0].contains("respond"),
            "error must name should_inject/respond, got {:?}",
            report.errors
        );
    }

    // AC (the issue's headline case): a syntax-valid script whose only function is misnamed
    // fails, naming the expected entrypoint.
    #[test]
    fn check_raw_misnamed_entrypoint_fails_naming_respond() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", "fn respnod(ctx) { pass() }");
        let report = run_check(&path, "respond").expect("check runs");
        assert!(!report.is_ok(), "misnamed entrypoint must fail");
        assert!(
            report.errors[0].contains('`') && report.errors[0].contains("respond"),
            "error must name the expected entrypoint, got {:?}",
            report.errors
        );
    }

    // AC: a syntax error fails.
    #[test]
    fn check_raw_syntax_error_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", "fn respond(ctx { pass() }");
        let report = run_check(&path, "respond").expect("check runs");
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("Syntax error"));
    }

    #[test]
    fn check_unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.txt", "whatever");
        let err = run_check(&path, "respond").unwrap_err();
        assert!(err.to_string().contains("extension"));
    }

    #[test]
    fn check_missing_file_errors() {
        let err = run_check(Path::new("/no/such/file.rhai"), "respond").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn check_raw_js_misnamed_entrypoint_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.js", "function respnod(ctx) { return pass(); }");
        let report = run_check(&path, "respond").expect("check runs");
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("respond"));
    }

    // ----- check: config -----

    #[test]
    fn check_config_reports_entrypoint_mismatch_with_location() {
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({
            "port": 4545,
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "rhai", "code": "fn respnod(ctx) { pass() }" } }
                }]
            }]
        });
        let path = write_temp(&dir, "imposter.json", &config.to_string());
        let report = run_check(&path, "respond").expect("check runs");
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("stubs[0].responses[0]"));
        assert!(report.errors[0].contains("respond"));
    }

    // Issue #360: state-used-without-flowState warning (rift-lint E042's check), reused here.
    #[test]
    fn check_config_warns_state_without_flow_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({
            "port": 4545,
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": {
                        "script": {
                            "engine": "rhai",
                            "code": "fn respond(ctx) { ctx.state.incr(\"n\"); pass() }"
                        }
                    }
                }]
            }]
        });
        let path = write_temp(&dir, "imposter.json", &config.to_string());
        let report = run_check(&path, "respond").expect("check runs");
        assert!(report.is_ok(), "state usage alone isn't an error");
        assert!(
            report.warnings.iter().any(|w| w.contains("flowState")),
            "expected a flowState warning, got {:?}",
            report.warnings
        );
    }

    #[test]
    fn check_config_no_warning_when_flow_state_configured() {
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({
            "port": 4545,
            "protocol": "http",
            "_rift": { "flowState": { "ttlSeconds": 300 } },
            "stubs": [{
                "responses": [{
                    "_rift": {
                        "script": {
                            "engine": "rhai",
                            "code": "fn respond(ctx) { ctx.state.incr(\"n\"); pass() }"
                        }
                    }
                }]
            }]
        });
        let path = write_temp(&dir, "imposter.json", &config.to_string());
        let report = run_check(&path, "respond").expect("check runs");
        assert!(report.is_ok());
        assert!(report.warnings.is_empty(), "got {:?}", report.warnings);
    }

    // ----- run -----

    // AC (issue #360): the fail-twice fixture with --state attempts=2 prints the 200 (pass)
    // branch; attempts=1 prints the 503 branch.
    #[test]
    fn run_fail_twice_fixture_attempts_2_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "fail-twice.rhai",
            r#"
                fn respond(ctx) {
                    let attempts = ctx.state.get_or("attempts", 0);
                    if attempts < 2 {
                        http(503, `attempt ${attempts}`)
                    } else {
                        pass()
                    }
                }
            "#,
        );
        let report = run_run(
            &path,
            None,
            &["attempts=2".to_string()],
            "cli",
            None,
            "respond",
        )
        .expect("run succeeds");
        assert_eq!(report.decision, "pass()");
        assert!(report.error.is_none());
    }

    #[test]
    fn run_fail_twice_fixture_attempts_1_fails_with_503() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "fail-twice.rhai",
            r#"
                fn respond(ctx) {
                    let attempts = ctx.state.get_or("attempts", 0);
                    if attempts < 2 {
                        http(503, `attempt ${attempts}`)
                    } else {
                        pass()
                    }
                }
            "#,
        );
        let report = run_run(
            &path,
            None,
            &["attempts=1".to_string()],
            "cli",
            None,
            "respond",
        )
        .expect("run succeeds");
        assert!(
            report.decision.starts_with("http(503)"),
            "got {}",
            report.decision
        );
    }

    // Issue #360 Item 2: mutated state and duration are reported.
    #[test]
    fn run_reports_mutated_state_and_duration() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.rhai",
            r#"fn respond(ctx) { ctx.state.set("seen", true); ctx.state.incr("hits"); pass() }"#,
        );
        let report = run_run(&path, None, &[], "cli", None, "respond").expect("run succeeds");
        assert_eq!(report.decision, "pass()");
        let state: HashMap<_, _> = report.state.into_iter().collect();
        assert_eq!(state.get("seen"), Some(&serde_json::json!(true)));
        assert_eq!(state.get("hits"), Some(&serde_json::json!(1)));
    }

    // Issue #360 Item 2: ctx.logger output is captured in the report.
    #[test]
    fn run_captures_logger_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.rhai",
            r#"fn respond(ctx) { ctx.logger.info("hello from script"); pass() }"#,
        );
        let report = run_run(&path, None, &[], "cli", None, "respond").expect("run succeeds");
        assert_eq!(report.logs, vec!["hello from script".to_string()]);
    }

    #[test]
    fn run_supports_http_result_constructor() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.rhai",
            r#"fn respond(ctx) { http(429, "slow down") }"#,
        );
        let report = run_run(&path, None, &[], "cli", None, "respond").expect("run succeeds");
        assert!(
            report.decision.starts_with("http(429)"),
            "got {}",
            report.decision
        );
    }

    // Issue #453: v1 `should_inject` was removed — `script run` surfaces the same
    // misnamed-entrypoint error as `script check` for a should_inject-only script.
    #[test]
    fn run_should_inject_only_script_errors_as_misnamed_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "s.rhai",
            r#"fn should_inject(request, flow_store) { #{ inject: true, fault: "error", status: 429, body: "slow down" } }"#,
        );
        let report = run_run(&path, None, &[], "cli", None, "respond")
            .expect("run reports an error, not a hard failure");
        assert!(
            report.error.is_some(),
            "expected an entrypoint error, got decision {:?}",
            report.decision
        );
        let err = report.error.unwrap();
        assert!(
            err.contains("entrypoint") && err.contains("respond"),
            "got {err}"
        );
    }

    #[test]
    fn run_uses_request_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_temp(
            &dir,
            "s.rhai",
            r#"fn respond(ctx) { if ctx.request.path == "/widgets" { http(201) } else { pass() } }"#,
        );
        let request = write_temp(&dir, "req.json", r#"{"method":"GET","path":"/widgets"}"#);
        let report =
            run_run(&script, Some(&request), &[], "cli", None, "respond").expect("run succeeds");
        assert!(
            report.decision.starts_with("http(201)"),
            "got {}",
            report.decision
        );
    }

    #[test]
    fn run_unsupported_hook_is_a_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", "fn matches(ctx) { true }");
        let err = run_run(&path, None, &[], "cli", None, "matches").unwrap_err();
        assert!(err.to_string().contains("matches"));
    }

    #[test]
    fn run_infers_engine_from_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", "fn respond(ctx) { pass() }");
        let report = run_run(&path, None, &[], "cli", None, "respond").expect("run succeeds");
        assert_eq!(report.decision, "pass()");
    }

    // A script that raises is reported, not propagated as a hard CLI error — duration/logs are
    // still meaningful for a failed run.
    #[test]
    fn run_script_error_is_reported_not_propagated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "s.rhai", r#"fn respond(ctx) { throw "boom" }"#);
        let report = run_run(&path, None, &[], "cli", None, "respond").expect("run succeeds");
        assert!(report.error.is_some());
        assert!(report.error.unwrap().contains("boom"));
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn run_js_fail_twice_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(
            &dir,
            "fail-twice.js",
            r#"
                function respond(ctx) {
                    var attempts = ctx.state.getOr("attempts", 0);
                    if (attempts < 2) {
                        return http(503, "attempt " + attempts);
                    }
                    return pass();
                }
            "#,
        );
        let report = run_run(
            &path,
            None,
            &["attempts=2".to_string()],
            "cli",
            None,
            "respond",
        )
        .expect("run succeeds");
        assert_eq!(report.decision, "pass()");
    }
}
