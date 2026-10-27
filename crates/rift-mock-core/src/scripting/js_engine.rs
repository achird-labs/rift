use crate::extensions::flow_state::{CasOutcome, FlowStore, flow_result};
use crate::imposter::{ResponseMode, ScriptFileAccess};
use crate::scripting::{
    FaultDecision, PredicateGeneratorError, ScriptCtxExtras, ScriptCtxInput, ScriptRequest,
    ScriptResult, ScriptResultBody, ScriptScope, ScriptStubContext, entrypoints,
};
use anyhow::{Result, anyhow};
use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsValue, Source, js_string,
    native_function::NativeFunction, object::builtins::JsArray, property::PropertyKey,
};

/// Create a JavaScript object with proper Object.prototype
/// This ensures the object has toString, valueOf, etc. for proper JS operations
fn create_js_object(context: &Context) -> JsObject {
    JsObject::with_object_proto(context.intrinsics())
}
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::sync::Arc;

// Thread-local storage for the flow store during script execution
thread_local! {
    static CURRENT_FLOW_STORE: RefCell<Option<Arc<dyn FlowStore>>> = const { RefCell::new(None) };
}

// Thread-local registry backing the v2 result-constructor values (`http`/`delay`/`reset`/`pass`,
// issue #357 Item 3). Boa's `JsObject` is a plain property bag with no easy way to embed an
// opaque Rust value, so each constructor stashes its [`ScriptResult`] here under a fresh id and
// hands the script a `{ __riftResultId: id }` object; `.header()` and the final conversion look
// the value back up by id (mirroring the `CURRENT_FLOW_STORE` thread-local pattern above).
thread_local! {
    static SCRIPT_RESULT_REGISTRY: RefCell<std::collections::HashMap<u64, ScriptResult>> =
        RefCell::new(std::collections::HashMap::new());
    static SCRIPT_RESULT_NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Set the current flow store for thread-local access
fn set_current_flow_store(store: Arc<dyn FlowStore>) {
    CURRENT_FLOW_STORE.with(|s| {
        *s.borrow_mut() = Some(store);
    });
}

/// Clear the current flow store
fn clear_current_flow_store() {
    CURRENT_FLOW_STORE.with(|s| {
        *s.borrow_mut() = None;
    });
}

/// Get the current flow store
fn with_current_flow_store<T>(f: impl FnOnce(&Arc<dyn FlowStore>) -> T) -> Option<T> {
    CURRENT_FLOW_STORE.with(|s| s.borrow().as_ref().map(f))
}

/// JavaScript script engine for fault injection using Boa Engine
///
/// # Script Interface (v2 `ctx` API, issue #357; the pre-1.0 v1 `should_inject` contract was
/// removed in issue #453)
///
/// Scripts define a `respond` function (or a bare expression with no function declarations at
/// all) taking a single `ctx` argument:
///
/// ```javascript
/// function respond(ctx) {
///     // Your logic here
///     return pass();
/// }
/// ```
///
/// ## `ctx.request`
///
/// - `ctx.request.method` - HTTP method (string): "GET", "POST", "PUT", "DELETE", etc.
/// - `ctx.request.path` - Request path (string): "/api/users/123"
/// - `ctx.request.headers` - Object of (lowercased) header name to value
/// - `ctx.request.header(name)` - Case-insensitive header getter
/// - `ctx.request.body` - Raw request body (string)
/// - `ctx.request.json` - Lazily parsed JSON body (null if not valid JSON)
/// - `ctx.request.query` - Object of query parameter name to value
/// - `ctx.request.pathParams` - Object of path parameters extracted from route patterns
///
/// ## `ctx.state` (flow-scoped storage)
///
/// `ctx.state` is already scoped to the current flow id:
/// - `ctx.state.get(key)` - Get a stored value (returns null if not found)
/// - `ctx.state.set(key, value)` - Store a value (returns boolean)
/// - `ctx.state.exists(key)` - Check if key exists (returns boolean)
/// - `ctx.state.delete(key)` - Delete a key (returns boolean)
/// - `ctx.state.incr(key)` - Increment counter (returns number)
/// - `ctx.state.getOr(key, default)` - Get a value, or `default` if absent
/// - `ctx.state.incrBy(key, n)` - Atomic increment by `n`, starting at 0 when absent
/// - `ctx.state.cas(key, expected, new)` - Atomic compare-and-set
/// - `ctx.state.ttl(seconds)` - Set flow expiration
///
/// `ctx.store.flow(flowId)` returns the same handle scoped to an arbitrary (not necessarily the
/// request's own) flow id.
///
/// ## Return Value
///
/// The function returns one of the result constructors, or nothing (equivalent to `pass()`):
///
/// ```javascript
/// // No fault injection
/// pass()
///
/// // Latency injection
/// delay(500)
///
/// // Error injection
/// http(503, "Service unavailable")
///
/// // Error with custom headers and a JSON body
/// http(429, { error: "Rate limited" }).header("Retry-After", "60")
/// ```
///
/// ## Example
///
/// ```javascript
/// function respond(ctx) {
///     // Rate limit based on the resolved flow id
///     var attempts = ctx.state.incr("attempts");
///     if (attempts > 3) {
///         return http(429, "Rate limited");
///     }
///
///     // Inject fault for POST requests to specific path
///     if (ctx.request.method === "POST" && ctx.request.path === "/api/test") {
///         return delay(100);
///     }
///
///     return pass();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct JsEngine {
    script: String,
    rule_id: String,
}

impl JsEngine {
    pub fn new(script: &str, rule_id: &str) -> Result<Self> {
        // Validate the script PARSES — not that it runs. Issue #357 Item 2 legalizes bare-
        // expression scripts, which reference a top-level `ctx` that only exists at real
        // execution time; fully evaluating an unbound bare script here would spuriously fail with
        // "ctx is not defined" and, for a wrapper-form script, would run its top-level side
        // effects at construction time, not request time. `Script::parse` catches genuine syntax
        // errors without executing anything.
        let mut context = sealed_js_context();
        boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut context)
            .map_err(|e| anyhow!("Failed to compile JavaScript script: {e}"))?;

        Ok(Self {
            script: script.to_string(),
            rule_id: rule_id.to_string(),
        })
    }

    /// Execute the `respond(ctx)` entrypoint (or bare-expression script) and determine if a fault
    /// should be injected — see
    /// [`crate::scripting::ScriptEngine::should_inject_fault`] for the full contract.
    pub fn should_inject(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        self.should_inject_with_ctx(request, flow_store, &ScriptCtxExtras::default())
    }

    /// As [`Self::should_inject`], but with real `ctx.flowId`/`ctx.stub` context (issue #357
    /// Item 1).
    pub fn should_inject_with_ctx(
        &self,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        extra: &ScriptCtxExtras,
    ) -> Result<FaultDecision> {
        let ctx_input = extra.build_ctx_input(request);
        execute_js_script(&self.script, flow_store, &self.rule_id, &ctx_input)
    }
}

/// Compile JavaScript to bytecode for caching
/// Returns serialized bytecode that can be loaded efficiently
/// Note: Boa doesn't support bytecode serialization yet, so we store the source
/// and validate it compiles
// Used by proxy.rs but cross-module analysis doesn't see it
pub fn compile_js_to_bytecode(script: &str) -> Result<Vec<u8>> {
    // Validate the script PARSES (not that it runs) — see the comment on `JsEngine::new` for why
    // a bare v2 script can't be blind-evaluated here (issue #357 Items 2/4).
    let mut context = sealed_js_context();
    boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut context)
        .map_err(|e| anyhow!("Failed to compile JavaScript script: {e}"))?;

    // Store the source as "bytecode" since Boa doesn't support
    // serialized bytecode yet
    Ok(script.as_bytes().to_vec())
}

/// Execute JavaScript with a fresh context
/// This is used by the script pool workers
pub fn execute_js_bytecode(
    bytecode: &[u8],
    request: &ScriptRequest,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
) -> Result<FaultDecision> {
    let script =
        std::str::from_utf8(bytecode).map_err(|e| anyhow!("Invalid UTF-8 in bytecode: {e}"))?;
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(request);
    execute_js_script(script, flow_store, rule_id, &ctx_input)
}

/// Internal function to execute JavaScript script
fn execute_js_script(
    script: &str,
    flow_store: Arc<dyn FlowStore>,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
) -> Result<FaultDecision> {
    // Set the flow store in thread-local storage for native functions to access
    set_current_flow_store(flow_store);

    // Ensure we clear the flow store when done (even on error)
    let result = execute_js_script_inner(script, rule_id, ctx_input);

    clear_current_flow_store();

    result
}

/// Loop-iteration cap for a `respond(ctx)` Boa context (issue #327). Boa exposes no
/// per-instruction interrupt, so a runaway loop (`while(true){}`) can't observe the deadline abort
/// flag and would otherwise run its `spawn_blocking` thread forever. This cap makes a single
/// runaway loop throw once the limit is hit, so the execution returns `Err` and the thread is
/// freed. Generous enough that no realistic boolean fault-decision script approaches it.
///
/// Known limitation (issue #371): Boa's loop-iteration counter is **per-call-frame** (reset to 0 on
/// every function call), and Boa 0.20 has no cumulative-work budget or per-instruction interrupt.
/// So a *nested* runaway — e.g. `while (true) { f(); }` where `f` loops — amplifies by roughly
/// `limit^depth` before the outermost loop trips, occupying its thread far longer than a flat
/// `while(true){}`. This is bounded, not infinite, and the client is still released at the
/// wall-clock timeout with a 500; only a background thread lingers, and only for a deliberately
/// adversarial *trusted* (operator-authored) script. Deep recursion is separately bounded by Boa's
/// default `recursion_limit` (512). Fully closing the loop-amplification would need a Boa fuel /
/// interrupt mechanism that 0.20 lacks.
const JS_SCRIPT_LOOP_ITERATION_LIMIT: u64 = 10_000_000;

/// Recursion depth cap applied to every bounded Boa `Context` (issue #355 Item 3). Boa's own
/// default is already 512; set explicitly so the contract doesn't silently drift if Boa's default
/// ever changes.
const JS_SCRIPT_RECURSION_LIMIT: usize = 512;

/// Stack size cap (bytes) applied to every bounded Boa `Context` (issue #355 Item 3), mirroring
/// Boa's own default (10 * 1024).
const JS_SCRIPT_STACK_SIZE_LIMIT: usize = 10 * 1024;

/// Build a `Context` with the interpreter-level guards every Mountebank JS hook (inject response,
/// predicate inject, `predicateGenerators.inject`, decorate) must run under (issue #355 Items 2/3).
///
/// Boa 0.20 has no per-instruction wall-clock interrupt, so these MB hooks — which execute
/// synchronously, inline in the request path, with no `spawn_blocking` wrapper — can't honor a
/// wall-clock deadline the way `_rift.script` does (see `bounded.rs::should_inject_bounded`).
/// The loop-iteration cap is what makes a `while(true){}` throw instead of hanging its thread; the
/// recursion/stack-size limits catch runaway recursion the same way. This is the enforcement layer
/// that stands in for the `_rift.script` wall-clock budget (`bounded::DEFAULT_SCRIPT_TIMEOUT_MS`)
/// for these sync call sites — every one of them must build its `Context` through this helper
/// rather than `Context::default()`.
///
/// It carries the hooks' host globals too ([`install_host_globals`]; port 0, as no imposter is
/// known), so a `wait` function may log or set a timer. Nothing runs its jobs: a timer it sets
/// never fires, and a promise it returns is not awaited, as in Mountebank's `wait` (issue #1593).
pub(crate) fn bounded_js_context() -> Context {
    let mut context = sealed_js_context();
    bound_js_context(&mut context);
    if let Err(e) = install_host_globals(&mut context, 0, None) {
        // Not reached: setting a property on a fresh global object cannot fail. A context
        // without them is still bounded; a script using one throws a ReferenceError.
        tracing::error!("could not set the script host globals: {e:#}");
    }
    context
}

/// A [`ContextBuilder`](boa_engine::context::ContextBuilder) whose context loads no module. Boa's
/// default loader resolves `import()` from the working directory, so a script of any imposter,
/// whatever its `require()` access, could read any JS or JSON file under it (review of #1645).
/// Every Boa context rift builds starts here, and is finished by [`build_sealed_js_context`].
fn sealed_js_context_builder() -> boa_engine::context::ContextBuilder {
    boa_engine::context::ContextBuilder::new()
        .module_loader(std::rc::Rc::new(boa_engine::module::IdleModuleLoader))
}

/// Build a sealed context, then remove `Atomics.waitAsync` from it (review of #1593). A waiter
/// that call registers is linked into Boa's process-wide futex list, with its `SharedArrayBuffer`,
/// and only a notify or its own timeout unlinks it. A hook's run drops both when it ends, so every
/// such call would leak its buffer for the life of the process (256 MiB a request for
/// `new SharedArrayBuffer(1 << 28)`). No Mountebank script needs it. `Atomics.wait` stays: a
/// context built here cannot block (`[[CanBlock]]` is false), so it throws.
fn build_sealed_js_context(builder: boa_engine::context::ContextBuilder) -> Context {
    let mut context = builder
        .build()
        // As `Context::default()` does: building a context with the default realm cannot fail.
        .expect("building a Boa context with the default realm cannot fail");
    let removed = context
        .global_object()
        .get(js_string!("Atomics"), &mut context)
        .ok()
        .and_then(|atomics| atomics.as_object())
        .map(|atomics| atomics.delete_property_or_throw(js_string!("waitAsync"), &mut context));
    // Fail closed: a context that kept `waitAsync` must not run scripts. Not reached: Boa defines
    // `Atomics` and its methods as configurable data properties of a fresh realm.
    assert!(
        matches!(removed, Some(Ok(_))),
        "could not remove Atomics.waitAsync from a script context"
    );
    context
}

/// A context with Boa's defaults except the module loader and `Atomics.waitAsync`: see
/// [`build_sealed_js_context`].
pub(crate) fn sealed_js_context() -> Context {
    build_sealed_js_context(sealed_js_context_builder())
}

/// [`sealed_js_context`] running its jobs on `jobs` (issue #1593).
pub(crate) fn sealed_js_context_on(jobs: std::rc::Rc<super::js_runtime::MbJobExecutor>) -> Context {
    build_sealed_js_context(sealed_js_context_builder().job_executor(jobs))
}

/// Apply the loop, recursion and stack limits every script context runs under (issue #327).
fn bound_js_context(context: &mut Context) {
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(JS_SCRIPT_LOOP_ITERATION_LIMIT);
    limits.set_recursion_limit(JS_SCRIPT_RECURSION_LIMIT);
    limits.set_stack_size_limit(JS_SCRIPT_STACK_SIZE_LIMIT);
}

/// Cap on distinct parsed scripts retained per MB pool worker (issue #476). Same clear-when-full
/// policy as the Rhai `compiled_cache` (#356 B3): comfortably above any realistic imposter
/// working set, while bounding memory against a churny one.
const MB_SCRIPT_CACHE_CAPACITY: usize = 64;

/// Per-worker reused Boa realm + parsed-`Script` cache for the Mountebank JS hooks (issue #476).
///
/// Building a fresh `Context::default()` constructs the entire JS realm (intrinsics, prototypes)
/// and `context.eval` re-parses the source — per call, previously the dominant cost of a trivial
/// MB hook. Boa has no bytecode serialization and a parsed [`boa_engine::Script`] is a `Gc`
/// handle tied to the realm it was parsed in, so the amortization unit is the pair: one context
/// per pool worker, plus that context's parsed scripts keyed by source hash.
///
/// Owned by each MB pool worker as a **loop local**, never as a Rust `thread_local` — a Boa
/// `Context` must not be dropped from a TLS destructor: `boa_gc`'s own thread-local heap can be
/// destroyed first, and the `Gc` handles inside the context then panic in a destructor (process
/// abort). The worker loop drops it on a live thread instead.
///
/// A worker keeps one realm per [`ScriptFileAccess`] it has run hooks under (issue #1645): a hook
/// only ever runs in the realm of its own imposter's access. Reusing a realm means one script's
/// writes to JS globals are visible to later scripts of the same access on that worker —
/// Mountebank parity: MB evals every injection in one shared Node process, so injected globals
/// leak there too. They never reach a run of another access, so a builtin a network-written
/// script patches, or a promise job it leaves queued, never runs inside a trusted run or in that
/// of a tenant with another root (rift-cluster). Imposters of one access still share globals, as in
/// Mountebank: none can borrow more file access from another, but one can see what another's
/// script passes through a builtin it patched. A run's inputs (`config`, `logger`, `state`,
/// `require`) are never globals: each run passes its own into the hook's scope (issues #1611,
/// #1678), the globals the hooks contract on (`console`, the timers) are set again per run
/// ([`MbRun`]), and the per-imposter state object comes from the run's [`ScriptScope`], the
/// imposter's own cell (issue #1726), so cross-imposter isolation of *state* is unaffected. A
/// `require` a script keeps answers only during the run that made it.
///
/// A worker is an event loop (issue #1678), as Node is: a run waiting on a promise job or a timer
/// stays in its realm ([`MbRealm::runs`]) while the worker starts other runs and runs whatever job
/// is ready, so a waiting run holds no worker.
struct MbJsThread {
    /// Most recently used first; at most [`MB_REALMS_PER_WORKER`] beside those with a run in
    /// flight, which are never dropped.
    realms: Vec<MbRealm>,
    /// Runs waiting to start until their imposter has no run in flight in another realm
    /// ([`MbJsThread::split_from`]), tried again after every step.
    deferred: Vec<DeferredStart>,
}

/// A run start put off on its worker ([`MbJsThread::deferred`]).
type DeferredStart = Box<dyn FnOnce(&mut MbJsThread)>;

/// Distinct [`ScriptFileAccess`] realms one MB worker keeps (issue #1645). Plain rift has at most
/// three (trusted, confined to `--scripts-dir`, refused); an embedder with a root per tenant
/// rebuilds the least recently used realm past this.
const MB_REALMS_PER_WORKER: usize = 8;

/// Jobs a realm runs before its worker looks at its queue again (issue #1678): an endless chain of
/// promise jobs delays requests new to the worker by at most this many jobs. The chain still ends
/// at the deadline of the run that started it.
const MB_JOBS_PER_STEP: usize = 256;

/// Requests a worker starts before it runs the jobs of the runs already in flight (issue #1678).
const MB_STARTS_PER_STEP: usize = 64;

/// How often a worker with a run in flight looks for runs nobody waits for any more, when no
/// timer or deadline wakes it sooner (issue #1678). Such a run is dropped: it is what bounds a run
/// that never answers, rather than its script timeout, which a config may set as high as it likes.
const MB_ABANDON_POLL: std::time::Duration = std::time::Duration::from_millis(100);

impl MbJsThread {
    fn new() -> Self {
        Self {
            realms: Vec::new(),
            deferred: Vec::new(),
        }
    }

    /// Whether `scope`'s imposter has a run in flight in a realm other than `files`'s: its script
    /// file access changed meanwhile (`ImposterCore::set_script_file_access`). Its next run must
    /// wait for those to end, as two realms would hold two live `state` objects and lose updates.
    fn split_from(&self, scope: &ScriptScope, files: &ScriptFileAccess) -> bool {
        let key = state_key(scope);
        self.realms
            .iter()
            .any(|realm| realm.access != *files && realm.live_state.contains_key(&key))
    }

    /// The realm for hooks running under `files` (issue #1645).
    fn realm(&mut self, files: &ScriptFileAccess) -> &mut MbRealm {
        match self.realms.iter().position(|realm| realm.access == *files) {
            Some(0) => {}
            Some(i) => {
                let realm = self.realms.remove(i);
                self.realms.insert(0, realm);
            }
            None => {
                if self.realms.len() >= MB_REALMS_PER_WORKER
                    && let Some(idle) = self.realms.iter().rposition(|realm| realm.runs.is_empty())
                {
                    self.realms.remove(idle);
                }
                self.realms.insert(0, MbRealm::new(files.clone()));
            }
        }
        &mut self.realms[0]
    }

    fn has_runs(&self) -> bool {
        !self.deferred.is_empty() || self.realms.iter().any(|realm| !realm.runs.is_empty())
    }

    /// Run the ready jobs of the runs in flight, end the runs that are done, and try the deferred
    /// starts again.
    fn step(&mut self) {
        for realm in &mut self.realms {
            if !realm.runs.is_empty() {
                realm.step();
            }
        }
        for start in std::mem::take(&mut self.deferred) {
            start(self);
        }
    }

    /// When a run in flight next needs the worker: a ready job, a timer, a deadline, or the next
    /// look for abandoned runs.
    fn next_wake(&self) -> std::time::Instant {
        let poll = std::time::Instant::now() + MB_ABANDON_POLL;
        self.realms
            .iter()
            .filter_map(MbRealm::next_wake)
            .fold(poll, std::time::Instant::min)
    }

    /// Drive every run in flight to its end on this thread: a test's worker, or a run nested in a
    /// worker's own job ([`run_blocking`]). A pool worker never calls this.
    fn drive(&mut self) {
        loop {
            self.step();
            if !self.has_runs() {
                return;
            }
            std::thread::sleep(
                self.next_wake()
                    .saturating_duration_since(std::time::Instant::now()),
            );
        }
    }
}

/// One access's reused Boa context, its parsed scripts, and the runs in flight in it (issues #476,
/// #1645, #1678).
struct MbRealm {
    access: ScriptFileAccess,
    context: Context,
    scripts: MbScriptCache,
    /// The context's job executor: its jobs run only through [`MbRealm::step`] (issue #1593).
    jobs: std::rc::Rc<super::js_runtime::MbJobExecutor>,
    /// The timer functions over the realm's one timer table, set again at every run start.
    timers: super::js_runtime::TimerGlobals,
    /// The runs waiting on a promise job or a timer (issue #1678), in start order.
    runs: Vec<InFlight>,
    /// The live `state` object of each imposter with a run in this realm (issue #1678): all its
    /// runs share it, as Mountebank's share `imposterState`, so a write is visible to a waiting run
    /// at its next `await`, and a synchronous section never interleaves with another run. Saved to
    /// the imposter's cell whenever one of its runs ends, and dropped when the last one does, so
    /// the cell is current whenever no run of the imposter is in flight. The pool routes every run
    /// of an imposter to the one worker holding its runs ([`MbPool::claim`]).
    live_state: std::collections::HashMap<StateKey, JsValue>,
}

impl MbRealm {
    fn new(access: ScriptFileAccess) -> Self {
        let executor = std::rc::Rc::new(super::js_runtime::MbJobExecutor::new());
        let mut context = sealed_js_context_on(std::rc::Rc::clone(&executor));
        bound_js_context(&mut context);
        // Not reached: setting a property on a fresh global object cannot fail.
        let timers = super::js_runtime::install_timers(&mut context)
            .expect("setting the timer globals of a fresh context cannot fail");
        Self {
            access,
            context,
            scripts: std::collections::HashMap::new(),
            jobs: executor,
            timers,
            runs: Vec::new(),
            live_state: std::collections::HashMap::new(),
        }
    }

    /// The `state` object `scope`'s imposter's runs in this realm share: the live one while a run
    /// of it is in flight, else a new one built from its cell.
    fn live_state(&mut self, scope: &ScriptScope) -> Result<JsValue> {
        let key = state_key(scope);
        if let Some(state) = self.live_state.get(&key) {
            return Ok(state.clone());
        }
        let seed = scope.state.lock().clone();
        let state = json_to_js(&mut self.context, &Value::Object(seed))?;
        self.live_state.insert(key, state.clone());
        Ok(state)
    }

    /// Save `scope`'s live state to its cell, and drop it when no run of the imposter is left.
    fn save_state(&mut self, scope: &ScriptScope, hook: &str) {
        let key = state_key(scope);
        let Some(state) = self.live_state.get(&key).cloned() else {
            return;
        };
        save_script_state(
            &mut self.context,
            &state,
            &mut scope.state.lock(),
            scope,
            hook,
        );
        let in_flight = self.runs.iter().any(|run| run.key() == Some(key));
        if !in_flight {
            self.live_state.remove(&key);
        }
    }

    /// Take in a run whose hook function has run: end it now when it is done, else keep it in
    /// flight until a job answers it, its jobs run out, its deadline passes or nobody waits.
    fn admit(&mut self, run: InFlight, started: Result<(), RunEnd>) {
        self.runs.push(run);
        let last = self.runs.len() - 1;
        match started {
            Err(end) => self.end(last, end),
            Ok(()) if self.answered(last) => self.end(last, RunEnd::Finished),
            Ok(()) => {}
        }
    }

    fn answered(&mut self, i: usize) -> bool {
        let InFlight { run, hook, .. } = &self.runs[i];
        let _active = run.activate(&mut self.context);
        hook.answered(&mut self.context)
    }

    /// Run up to [`MB_JOBS_PER_STEP`] ready jobs, each as the run that queued it, then end every
    /// run that is done.
    fn step(&mut self) {
        for _ in 0..MB_JOBS_PER_STEP {
            let Some((tag, job)) = self.jobs.pop_ready(std::time::Instant::now()) else {
                break;
            };
            // Every job of a run that ended was dropped with it, or passed on.
            let Some(i) = self.runs.iter().position(|run| tag == run.run.id) else {
                continue;
            };
            let ran = {
                let _active = self.runs[i].run.activate(&mut self.context);
                let previous = self.jobs.set_expiry(tag.expiry);
                let ran = job.run(&mut self.context);
                self.jobs.set_expiry(previous);
                ran
            };
            match ran {
                Err(e) => self.end(i, RunEnd::Threw(e)),
                Ok(()) if self.answered(i) => self.end(i, RunEnd::Finished),
                Ok(()) => {}
            }
        }
        self.context.clear_kept_objects();
        let mut i = 0;
        while i < self.runs.len() {
            match self.ending(i) {
                Some(end) => self.end(i, end),
                None => i += 1,
            }
        }
    }

    /// How run `i` ends now, if it does.
    fn ending(&mut self, i: usize) -> Option<RunEnd> {
        if self.runs[i].hook.abandoned() {
            return Some(RunEnd::Abandoned);
        }
        if self.answered(i) {
            return Some(RunEnd::Finished);
        }
        let deadline = self.runs[i].deadline;
        if std::time::Instant::now() >= deadline {
            return Some(RunEnd::Deadline);
        }
        let queued = self.jobs.queued_for(self.runs[i].run.id);
        if queued.ready > 0 {
            return None;
        }
        match queued.next_timer {
            // A run of the same imposter may still settle what this one waits for: a promise the
            // two share through `state`, as Mountebank's runs can.
            None if self.kin_have_jobs(i) => None,
            None => Some(RunEnd::Idle),
            // Its next timer is due after its deadline: it cannot answer in time (issue #1593).
            Some(due) if due > deadline => Some(RunEnd::Deadline),
            Some(_) => None,
        }
    }

    /// Whether another run of run `i`'s imposter in this realm has a job or timer queued.
    fn kin_have_jobs(&self, i: usize) -> bool {
        let Some(key) = self.runs[i].key() else {
            return false;
        };
        self.runs.iter().enumerate().any(|(j, run)| {
            j != i && run.key() == Some(key) && {
                let queued = self.jobs.queued_for(run.run.id);
                queued.ready > 0 || queued.next_timer.is_some()
            }
        })
    }

    /// End run `i`: its hook answers for `end`, its imposter's live state is saved, what it left
    /// queued is dropped, and its claim on the worker is released, in that order, so the cell is
    /// current before another worker can take the imposter's next run. When it answered while
    /// another run of its imposter is in flight in this realm, what it left goes to that run
    /// instead (it may be what that run waits for: a promise the two share), still to be dropped
    /// at this run's deadline, so nothing it queued outlives that (issue #1645). A predicate run's
    /// jobs always end with it, as Mountebank does not wait for a predicate.
    fn end(&mut self, i: usize, end: RunEnd) {
        let InFlight {
            run,
            deadline,
            hook,
            scope,
            claim,
        } = self.runs.remove(i);
        let name = hook.name();
        let bequeaths = matches!(end, RunEnd::Finished | RunEnd::Idle) && hook.bequeaths();
        {
            let _active = run.activate(&mut self.context);
            match end {
                RunEnd::Abandoned => drop(hook),
                end => hook.finish(&mut self.context, end),
            }
            if let Some(scope) = &scope {
                self.save_state(scope, name);
            }
        }
        let key = scope.as_ref().map(state_key);
        let heir = self
            .runs
            .iter()
            .find(|other| bequeaths && key.is_some() && other.key() == key)
            .map(|other| other.run.id);
        if let Some(heir) = heir {
            self.jobs.retag(run.id, heir, deadline);
            self.timers.retag(run.id, heir);
        }
        // The run's jobs go before its claim (see above).
        drop(run);
        drop(claim);
    }

    fn next_wake(&self) -> Option<std::time::Instant> {
        if self.runs.is_empty() {
            return None;
        }
        if self.jobs.has_ready() {
            return Some(std::time::Instant::now());
        }
        let deadlines = self.runs.iter().map(|run| run.deadline);
        self.jobs.next_due().into_iter().chain(deadlines).min()
    }
}

/// A run taken in by its realm ([`MbRealm::admit`]) until it ends.
struct InFlight {
    run: MbRun,
    deadline: std::time::Instant,
    hook: Box<dyn MbHook>,
    /// The imposter whose live state the run shares; `None` for a hook with no `state`.
    scope: Option<ScriptScope>,
    claim: Claim,
}

impl InFlight {
    fn key(&self) -> Option<StateKey> {
        self.scope.as_ref().map(state_key)
    }
}

/// How a run ends (issue #1678).
enum RunEnd {
    /// Its answer is ready.
    Finished,
    /// It has no job left and no answer.
    Idle,
    /// Its deadline passed, or its next timer is due after it.
    Deadline,
    /// One of its jobs threw.
    Threw(boa_engine::JsError),
    /// Building its inputs failed, or its hook function threw (each hook words its own error).
    Failed(anyhow::Error),
    /// Nobody waits for its answer any more: the caller timed out, or its request was cancelled
    /// (issue #1643). It is dropped without one.
    Abandoned,
}

impl From<anyhow::Error> for RunEnd {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

/// The part of a hook run that outlives its start (issue #1678): whether it has its answer, and
/// how to give it.
trait MbHook {
    fn name(&self) -> &'static str;
    /// Whether nobody waits for the answer any more: the caller timed out, or its request was
    /// cancelled (issue #1643).
    fn abandoned(&self) -> bool;
    fn answered(&self, context: &mut Context) -> bool;
    fn finish(self: Box<Self>, context: &mut Context, end: RunEnd);
    /// Whether what the run leaves queued when it answers may pass to another run of its imposter
    /// ([`MbRealm::end`]).
    fn bequeaths(&self) -> bool {
        true
    }
}

/// Where a hook run's answer goes: a caller awaiting it, or one blocked on it.
enum Reply<T> {
    Async(tokio::sync::oneshot::Sender<T>),
    Sync(std::sync::mpsc::SyncSender<T>),
}

impl<T> Reply<T> {
    fn send(self, answer: T) {
        // Domain-optional: a caller that stopped waiting (its timeout, a delete) wants no answer.
        match self {
            Self::Async(tx) => drop(tx.send(answer)),
            Self::Sync(tx) => drop(tx.send(answer)),
        }
    }

    fn abandoned(&self) -> bool {
        match self {
            Self::Async(tx) => tx.is_closed(),
            Self::Sync(_) => false,
        }
    }
}

/// An imposter's identity in the pool: its state cell's address (issue #1678). Unique while a run
/// holds the cell, which every claim keyed by it does; a `restart` swaps the cell, so the
/// imposter's next runs are a new key with a new live state.
type StateKey = usize;

fn state_key(scope: &ScriptScope) -> StateKey {
    Arc::as_ptr(&scope.state).addr()
}

/// Script-source → parsed-`Script` cache for one worker's MB context.
type MbScriptCache = std::collections::HashMap<u64, (String, boa_engine::Script)>;

/// Test-only observable proving the parsed-`Script` cache works (issue #476 AC5): total parses
/// (cache misses) per source, across all pool workers.
#[cfg(test)]
static MB_PARSE_LOG: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Total parses of every cached source containing `fragment` (test observable; the fragment
/// avoids coupling the test to the exact executor wrapper text).
#[cfg(test)]
pub(crate) fn mb_script_parse_count_containing(fragment: &str) -> u64 {
    MB_PARSE_LOG
        .lock()
        .iter()
        .filter(|(source, _)| source.contains(fragment))
        .map(|(_, n)| n)
        .sum()
}

/// Evaluate `source` against a worker's reused context, parsing at most once per distinct
/// source. A hit is source-verified (like `compiled_cache` #356 B4): a `u64` collision would
/// otherwise evaluate the WRONG script, so a mismatch re-parses instead.
fn eval_mb_script(
    context: &mut Context,
    scripts: &mut MbScriptCache,
    source: &str,
) -> JsResult<JsValue> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    let key = hasher.finish();

    if let Some((cached_source, script)) = scripts.get(&key)
        && cached_source == source
    {
        let script = script.clone();
        return script.evaluate(context);
    }

    let script = boa_engine::Script::parse(Source::from_bytes(source.as_bytes()), None, context)?;
    #[cfg(test)]
    {
        *MB_PARSE_LOG.lock().entry(source.to_string()).or_insert(0) += 1;
    }
    if scripts.len() >= MB_SCRIPT_CACHE_CAPACITY {
        scripts.clear();
    }
    scripts.insert(key, (source.to_string(), script.clone()));
    script.evaluate(context)
}

/// Evaluate a hook wrapper `(function (require, ...) { ... })` and call it with this run's
/// `require` and inputs. Each is a binding of the hook's scope, as `require` is in Node (issue
/// #1645), never a global: runs of one realm interleave (issue #1678), so a global would be
/// whichever run set it last.
fn call_mb_wrapper(
    context: &mut Context,
    scripts: &mut MbScriptCache,
    source: &str,
    args: &[JsValue],
) -> JsResult<JsValue> {
    let wrapper = eval_mb_script(context, scripts, source)?;
    let wrapper = wrapper.as_callable().ok_or_else(|| {
        JsNativeError::typ().with_message("the hook wrapper did not evaluate to a function")
    })?;
    wrapper.call(&JsValue::undefined(), args, context)
}

/// A unit of work for the MB script pool: runs on a worker with that worker's realms.
type MbJob = Box<dyn FnOnce(&mut MbJsThread) + Send>;

thread_local! {
    /// Set on MB pool worker threads, so a reentrant executor call (a native hook re-entering
    /// an executor mid-job) runs inline on a fresh context instead of deadlocking on its own
    /// worker's queue.
    static IS_MB_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// The dedicated worker pool that executes every Mountebank JS hook (issues #476, #1678). Workers
/// own their [`MbJsThread`] as a loop local (see its doc for why TLS is unsound here) and are
/// reused for the process lifetime, so realm construction and script parsing amortize across
/// requests. Half the cores, clamped to [4, 16]: a CPU-bound script holds its worker until the
/// loop-iteration cap (#327) throws, so with 2 workers two runaways would stall all MB scripting.
///
/// Each worker has its own queue, and every run of one imposter goes to one worker while any of
/// its runs is queued or in flight ([`MbPool::claim`]), where they share the imposter's live
/// `state` object. A new imposter's runs go to the worker with the fewest runs. A waiting run
/// holds no worker, so the pool is never starved by sleeping scripts; a CPU-bound one delays the
/// imposters that share its worker until the loop cap throws, where Mountebank's one event loop
/// stalls every imposter.
#[derive(Debug)]
struct MbPool {
    queues: Vec<crossbeam::channel::Sender<MbJob>>,
    /// Jobs of no imposter (`predicateGenerators.inject`), which need no worker of their own: the
    /// first worker free takes each, as no job queues behind a CPU-bound script there.
    shared: crossbeam::channel::Sender<MbJob>,
    /// Whether each worker is executing (not waiting for its queue or a timer), and the jobs sent
    /// to it that it has not started: a new imposter's runs go to an idle worker with an empty
    /// queue first, as the job ahead of them may be a CPU-bound script.
    busy: Vec<std::sync::atomic::AtomicBool>,
    queued: Vec<std::sync::atomic::AtomicUsize>,
    routes: parking_lot::Mutex<Routes>,
}

#[derive(Debug)]
struct Routes {
    /// Claims held on each worker: runs queued or in flight there.
    load: Vec<usize>,
    /// The worker every run of an imposter goes to, and how many claims are held for it.
    owners: std::collections::HashMap<StateKey, Owner>,
}

#[derive(Debug, Clone, Copy)]
struct Owner {
    worker: usize,
    claims: usize,
}

/// A run's place on a pool worker, held from submission until the run ends; dropping it releases
/// the place, and the imposter's route with its last claim. `None` for a run that holds no pool
/// place: one driven on a thread of its own ([`MbJsThread::drive`]), or of no imposter, which any
/// worker takes from the shared queue.
#[derive(Debug)]
#[must_use = "dropping a claim releases its worker place"]
struct Claim(Option<Place>);

#[derive(Debug)]
struct Place {
    pool: &'static MbPool,
    worker: usize,
    key: Option<StateKey>,
}

impl Claim {
    fn local() -> Self {
        Self(None)
    }

    #[cfg(test)]
    fn worker(&self) -> Option<usize> {
        self.0.as_ref().map(|place| place.worker)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(place) = self.0.take() {
            place.pool.release(place.worker, place.key);
        }
    }
}

impl MbPool {
    /// The worker a run goes to: its imposter's while the imposter has a claim, else an idle one
    /// with nothing queued and the fewest claims, else the one with the fewest.
    fn claim(&'static self, key: Option<StateKey>) -> Result<Claim> {
        let mut routes = self.routes.lock();
        let owned = key
            .and_then(|key| routes.owners.get_mut(&key))
            .map(|owner| {
                owner.claims += 1;
                owner.worker
            });
        let worker = match owned {
            Some(worker) => worker,
            None => {
                let worker = (0..self.queues.len())
                    .min_by_key(|&worker| {
                        let busy = self.busy[worker].load(std::sync::atomic::Ordering::Relaxed)
                            || self.queued[worker].load(std::sync::atomic::Ordering::Relaxed) > 0;
                        (busy, routes.load[worker])
                    })
                    .ok_or_else(|| anyhow!("MB script pool is unavailable"))?;
                if let Some(key) = key {
                    routes.owners.insert(key, Owner { worker, claims: 1 });
                }
                worker
            }
        };
        routes.load[worker] += 1;
        Ok(Claim(Some(Place {
            pool: self,
            worker,
            key,
        })))
    }

    fn release(&self, worker: usize, key: Option<StateKey>) {
        let mut routes = self.routes.lock();
        routes.load[worker] -= 1;
        if let Some(key) = key
            && let std::collections::hash_map::Entry::Occupied(mut owner) = routes.owners.entry(key)
        {
            owner.get_mut().claims -= 1;
            if owner.get().claims == 0 {
                owner.remove();
            }
        }
    }
}

fn mb_js_pool() -> &'static MbPool {
    static POOL: std::sync::OnceLock<MbPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let workers = (num_cpus::get() / 2).clamp(4, 16);
        let mut queues = Vec::with_capacity(workers);
        let (shared, shared_rx) = crossbeam::channel::unbounded::<MbJob>();
        for worker_id in 0..workers {
            let (tx, rx) = crossbeam::channel::unbounded::<MbJob>();
            let shared_rx = shared_rx.clone();
            // The index this worker's queue takes in `queues` (and its flag in `busy`).
            let index = queues.len();
            let spawned = std::thread::Builder::new()
                .name(format!("mb-js-worker-{worker_id}"))
                .spawn(move || run_mb_worker(index, &rx, &shared_rx));
            match spawned {
                Ok(_) => queues.push(tx),
                Err(e) => tracing::error!("failed to spawn MB script worker {worker_id}: {e}"),
            }
        }
        MbPool {
            busy: (0..queues.len())
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            queued: (0..queues.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            routes: parking_lot::Mutex::new(Routes {
                load: vec![0; queues.len()],
                owners: std::collections::HashMap::new(),
            }),
            queues,
            shared,
        }
    })
}

/// A pool worker's event loop (issue #1678): wait for a request, or until a run in flight needs
/// the worker; start what arrived; run the ready jobs; end the runs that are done.
fn run_mb_worker(
    index: usize,
    rx: &crossbeam::channel::Receiver<MbJob>,
    shared: &crossbeam::channel::Receiver<MbJob>,
) {
    IS_MB_WORKER.set(true);
    let busy = |now: bool| {
        mb_js_pool().busy[index].store(now, std::sync::atomic::Ordering::Relaxed);
    };
    let mut thread = MbJsThread::new();
    loop {
        busy(false);
        // The pool's senders live for the process, so a disconnected queue never happens; if it
        // did, the worker would have nothing left to serve.
        let first = if thread.has_runs() {
            let wait = thread
                .next_wake()
                .saturating_duration_since(std::time::Instant::now());
            crossbeam::channel::select! {
                recv(rx) -> job => match job { Ok(job) => Some(job), Err(_) => return },
                recv(shared) -> job => match job { Ok(job) => Some(job), Err(_) => return },
                default(wait) => None,
            }
        } else {
            crossbeam::channel::select! {
                recv(rx) -> job => match job { Ok(job) => Some(job), Err(_) => return },
                recv(shared) -> job => match job { Ok(job) => Some(job), Err(_) => return },
            }
        };
        busy(true);
        // A panicking job (an engine bug — scripts themselves error, not panic) must not kill the
        // worker; its context may be mid-mutation, so rebuild. The worker's runs in flight go with
        // it: their callers see the job fail, and their unsaved `state` writes are lost.
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(job) = first {
                job(&mut thread);
            }
            for job in rx.try_iter().take(MB_STARTS_PER_STEP) {
                job(&mut thread);
            }
            if let Ok(job) = shared.try_recv() {
                job(&mut thread);
            }
            thread.step();
        }));
        if let Err(panic) = turn {
            let payload = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a non-string panic".to_string());
            let lost: usize = thread.realms.iter().map(|realm| realm.runs.len()).sum();
            tracing::error!(
                "MB script job panicked ({payload}); rebuilding worker context, failing its {lost} \
                 runs in flight"
            );
            thread = MbJsThread::new();
        }
    }
}

/// Submit a job to the MB script pool, to the worker `key`'s imposter is claimed on, or to the
/// first free worker when `None`. `Err` only if the pool is gone (never in practice: the senders are process-lifetime
/// statics).
///
/// The caller's `tracing` dispatcher travels with the job, and a run started in it keeps it for
/// every job of its own ([`MbRun::activate`]), so script-logger events (issue #355) keep routing to
/// whatever subscriber was active at the call site — a thread-scoped capture included. (Span
/// *parentage* does not cross the pool boundary, like any thread hop; the logger events carry the
/// imposter port and stub id as explicit fields.)
fn submit_mb_job(
    key: Option<StateKey>,
    job: impl FnOnce(&mut MbJsThread, Claim) + Send + 'static,
) -> Result<()> {
    let pool = mb_js_pool();
    let Some(key) = key else {
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let wrapped: MbJob = Box::new(move |thread| {
            tracing::dispatcher::with_default(&dispatcher, || job(thread, Claim::local()));
        });
        return pool
            .shared
            .send(wrapped)
            .map_err(|_| anyhow!("MB script pool is unavailable"));
    };
    let claim = pool.claim(Some(key))?;
    let worker = claim.0.as_ref().map_or(0, |place| place.worker);
    let dispatcher = tracing::dispatcher::get_default(Clone::clone);
    pool.queued[worker].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let wrapped: MbJob = Box::new(move |thread| {
        pool.queued[worker].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::dispatcher::with_default(&dispatcher, || job(thread, claim));
    });
    pool.queues[worker].send(wrapped).map_err(|_| {
        pool.queued[worker].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        anyhow!("MB script pool is unavailable")
    })
}

/// The error of a caller whose pool job ended without an answer.
fn worker_panicked<E>(_: E) -> anyhow::Error {
    anyhow!("MB script job failed (worker panicked)")
}

/// Run `f` on an MB pool worker and block until it completes: a hook that never waits for a job
/// (`predicateGenerators.inject`). `Err` means the job infrastructure failed (worker panicked
/// mid-job, pool unavailable) — script errors travel inside `R`. On a reentrant call from a worker
/// itself, runs inline on a fresh context (dropped safely on the live thread).
fn with_mb_js_thread<R, F>(f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce(&mut MbJsThread) -> R + Send + 'static,
{
    if IS_MB_WORKER.get() {
        return Ok(f(&mut MbJsThread::new()));
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    submit_mb_job(None, move |thread, _claim| {
        let _ = tx.send(f(thread));
    })?;
    rx.recv().map_err(worker_panicked)
}

/// Start a hook run with `start` and block until it answers: the callers off the async runtime (a
/// `spawn_blocking` matcher thread, a test). The run goes to `scope`'s imposter's worker; on a
/// pool worker itself (a reentrant call), it runs to its end on a fresh worker state instead.
fn run_blocking<T: Send + 'static>(
    scope: &ScriptScope,
    start: impl FnOnce(&mut MbJsThread, Reply<Result<T>>, Claim) + Send + 'static,
) -> Result<T> {
    if IS_MB_WORKER.get() {
        return run_here(&mut MbJsThread::new(), start);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    submit_mb_job(Some(state_key(scope)), move |thread, claim| {
        start(thread, Reply::Sync(tx), claim);
    })?;
    rx.recv().map_err(worker_panicked)?
}

/// Start a hook run with `start` on `thread`, and drive the thread until it answers.
fn run_here<T>(
    thread: &mut MbJsThread,
    start: impl FnOnce(&mut MbJsThread, Reply<Result<T>>, Claim),
) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    start(thread, Reply::Sync(tx), Claim::local());
    thread.drive();
    rx.try_recv()
        .map_err(|_| anyhow!("the script run ended without an answer"))?
}

/// The top-level function names a JS script declares, for the static entrypoint check (issue
/// #360 Item 1). Two-phase, deliberately NOT a blind `context.eval` of the raw source:
///
///  1. **Syntax** — `Script::parse` catches genuine syntax errors WITHOUT executing anything
///     (exactly what [`JsEngine::new`] does). `Err` here is a real syntax error.
///  2. **Detection** — evaluate once with the SAME host globals the real execution path binds
///     (`ctx`/`http`/`delay`/`reset`/`pass`), under the SAME loop-iteration limit
///     ([`JS_SCRIPT_LOOP_ITERATION_LIMIT`]) that stops a top-level `while(true){}` from hanging.
///     Binding the globals is what keeps a legitimate #357 bare-expression response script (e.g.
///     `http(503, "boom")`, or `(ctx.request.method === "POST") ? http(503) : pass()`) from
///     spuriously throwing "http/ctx is not defined" — the very false-fail a blind unbound eval
///     caused. The eval's own Result is intentionally ignored: JS hoists every top-level
///     `function` declaration onto the global object BEFORE running statements, so the
///     declared-function set is complete even if a bare expression later throws at runtime — a
///     runtime throw is not this check's concern (matching Rhai, which never executes at all).
///
/// Returns the names of functions the SCRIPT itself declared (via a before/after global-key
/// diff), never the host builtins/globals bound above.
pub(crate) fn declared_functions_js(script: &str) -> Result<Vec<String>> {
    // Phase 1: syntax only.
    {
        let mut parse_ctx = sealed_js_context();
        boa_engine::Script::parse(Source::from_bytes(script.as_bytes()), None, &mut parse_ctx)
            .map_err(|e| anyhow!("Syntax error: {e}"))?;
    }

    // Phase 2: bounded eval with real globals, then diff for declared functions.
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());

    let dummy_request = ScriptRequest {
        mode: ResponseMode::Text,
        method: "GET".to_string(),
        path: "/".to_string(),
        headers: crate::util::OrderedMap::default(),
        body: Value::Null,
        query: crate::util::OrderedMap::default(),
        path_params: std::collections::HashMap::new(),
        raw_body: None,
    };
    let ctx_input = ScriptCtxExtras::default().build_ctx_input(&dummy_request);
    // A top-level `ctx.state.*` call needs a flow store bound; a no-op one is enough — the check
    // never inspects state, only which functions the script declared.
    set_current_flow_store(Arc::new(crate::extensions::flow_state::NoOpFlowStore));

    let mut context = sealed_js_context();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(JS_SCRIPT_LOOP_ITERATION_LIMIT);
    context
        .runtime_limits_mut()
        .set_recursion_limit(JS_SCRIPT_RECURSION_LIMIT);
    context
        .runtime_limits_mut()
        .set_stack_size_limit(JS_SCRIPT_STACK_SIZE_LIMIT);

    let ctx_obj = create_ctx_object(&mut context, &ctx_input)?;
    register_result_constructors(&mut context)?;

    let global = context.global_object();
    global
        .set(js_string!("ctx"), ctx_obj, false, &mut context)
        .map_err(|e| anyhow!("Failed to set ctx global: {e}"))?;

    let pre_existing: std::collections::HashSet<String> = global
        .own_property_keys(&mut context)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| match k {
            PropertyKey::String(s) => Some(s.to_std_string_escaped()),
            _ => None,
        })
        .collect();

    // Ignore the eval Result: a bare-expression script may throw at runtime (that's not this
    // check's job), while hoisted function declarations are already on the global regardless.
    let _ = context.eval(Source::from_bytes(script.as_bytes()));

    let mut names = Vec::new();
    for key in global.own_property_keys(&mut context).unwrap_or_default() {
        let name = match &key {
            PropertyKey::String(s) => s.to_std_string_escaped(),
            _ => continue,
        };
        if pre_existing.contains(&name) {
            continue;
        }
        if global
            .get(key, &mut context)
            .ok()
            .filter(JsValue::is_callable)
            .is_some()
        {
            names.push(name);
        }
    }
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());
    Ok(names)
}

/// Inner function that does the actual JavaScript execution
fn execute_js_script_inner(
    script: &str,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
) -> Result<FaultDecision> {
    execute_js_script_inner_bounded(script, rule_id, ctx_input, JS_SCRIPT_LOOP_ITERATION_LIMIT)
}

/// As [`execute_js_script_inner`], but with an explicit loop-iteration cap so tests can bound a
/// runaway cheaply (issue #327).
///
/// Dispatch (issue #357 Item 2): the script is evaluated once with `ctx` already set — its
/// completion value is the bare-expression result. Then: a global `respond` → named entrypoint
/// (call it with `ctx`). Else → bare (the completion value from the single eval above).
fn execute_js_script_inner_bounded(
    script: &str,
    rule_id: &str,
    ctx_input: &ScriptCtxInput,
    loop_iteration_limit: u64,
) -> Result<FaultDecision> {
    // B3 (issue #357): clear any `ScriptResult` registry entries left over from a previous run on
    // this reused `spawn_blocking` worker thread. Only the single RETURNED result's id is removed
    // in `js_value_to_fault_decision`; constructor calls that aren't the returned value (a
    // top-level `http(500)` completion, `var a = http(500); return pass();`, etc.) would otherwise
    // leak an entry forever. Reset per execution, mirroring the per-task flow-store reset.
    SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().clear());

    let mut context = sealed_js_context();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(loop_iteration_limit);

    let ctx_obj = create_ctx_object(&mut context, ctx_input)?;
    register_result_constructors(&mut context)?;

    let global = context.global_object();
    global
        .set(js_string!("ctx"), ctx_obj.clone(), false, &mut context)
        .map_err(|e| anyhow!("Failed to set ctx global: {e}"))?;

    // Snapshot the global own-property names present BEFORE eval, so afterwards we can tell which
    // function globals the SCRIPT itself declared (B1): Boa builtins plus the
    // ctx/http/delay/reset/pass globals we set above are all already present here.
    let pre_existing: std::collections::HashSet<String> = global
        .own_property_keys(&mut context)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| match k {
            PropertyKey::String(s) => Some(s.to_std_string_escaped()),
            _ => None,
        })
        .collect();

    let bare_result = context
        .eval(Source::from_bytes(script.as_bytes()))
        .map_err(|e| anyhow!("Failed to execute script: {e}"))?;

    let respond_fn = global
        .get(js_string!(entrypoints::RESPOND), &mut context)
        .ok()
        .filter(JsValue::is_callable);
    let result = if let Some(func) = respond_fn {
        func.as_callable()
            .ok_or_else(|| anyhow!("respond is not a function"))?
            .call(&JsValue::undefined(), &[ctx_obj], &mut context)
            .map_err(|e| anyhow!("Failed to call respond(ctx): {e}"))?
    } else {
        // B1 (issue #357, "nothing fails silently"): if the script DECLARED a function global
        // that isn't `respond` and produced no bare-expression value (undefined/null), it almost
        // certainly has a MISNAMED entrypoint (e.g. `function respnod(ctx)`). Falling through to
        // `bare_result` (→ `None`) would silently serve a normal response with no sign the script
        // never ran. Surface it as an explicit error instead. A genuine bare expression is still
        // fine: it either declared no new functions, or produced a non-undefined value.
        if bare_result.is_undefined() || bare_result.is_null() {
            let keys = global.own_property_keys(&mut context).unwrap_or_default();
            let mut declared_a_function = false;
            for key in keys {
                let name = match &key {
                    PropertyKey::String(s) => s.to_std_string_escaped(),
                    _ => continue,
                };
                if pre_existing.contains(&name) {
                    continue;
                }
                if global
                    .get(key, &mut context)
                    .ok()
                    .filter(JsValue::is_callable)
                    .is_some()
                {
                    declared_a_function = true;
                    break;
                }
            }
            if declared_a_function {
                return Err(anyhow!(
                    "script defines function(s) but none is the `respond` entrypoint \
                     (and there is no bare expression to evaluate); did you mean `respond`?"
                ));
            }
        }
        bare_result
    };

    js_value_to_fault_decision(&mut context, result, rule_id)
}

// Native function implementations that use thread-local storage

/// Map a `ctx.state` op outcome to a JS value: this ALWAYS raises a native error on a backend
/// failure (fail-loud is the whole point of the v2 `ctx.state` API, issue #358).
fn js_flow_outcome_strict<T: Into<JsValue>>(
    outcome: Option<std::result::Result<T, String>>,
) -> JsResult<JsValue> {
    match outcome {
        Some(Ok(v)) => Ok(v.into()),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// Register a native function method on a JS object.
fn register_method(
    obj: &JsObject,
    name: &str,
    func: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
    context: &mut Context,
) -> Result<()> {
    obj.set(
        PropertyKey::from(js_string!(name)),
        NativeFunction::from_fn_ptr(func).to_js_function(context.realm()),
        false,
        context,
    )
    .map(|_| ())
    .map_err(|e| anyhow!("Failed to set {name} method: {e}"))
}

// =============================================================================
// v2 `ctx` API (issue #357 Items 1-4)
// =============================================================================

fn clamp_u16_f64(n: f64) -> u16 {
    n.max(0.0).min(f64::from(u16::MAX)) as u16
}

fn parse_json_or_null(context: &mut Context, raw: &str) -> Result<JsValue> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => json_to_js(context, &v),
        Err(_) => Ok(JsValue::null()),
    }
}

/// Captures for the `ctx.request` case-insensitive `header(name)` getter: the
/// (non-lowercased) source headers, looked up case-insensitively at call time.
type HeaderGetterCaptures = std::collections::HashMap<String, Vec<String>>;

fn header_getter(
    _this: &JsValue,
    args: &[JsValue],
    captures: &HeaderGetterCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped());
    let found = name.and_then(|n| {
        captures
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&n))
            .map(|(_, v)| v)
    });
    // The same value `ctx.request.headers[name]` holds: an array for a repeated header (#1629).
    Ok(match found {
        Some(values) => values_to_js(context, values),
        None => JsValue::null(),
    })
}

fn set_header_getter(
    obj: &JsObject,
    headers: &crate::util::MultiValueMap,
    context: &mut Context,
) -> Result<()> {
    // A plain map for the captures: boa's GC traces std collections, and the lookup is by name.
    let captures: HeaderGetterCaptures = headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let native_fn = NativeFunction::from_copy_closure_with_captures(header_getter, captures)
        .to_js_function(context.realm());
    obj.set(js_string!("header"), native_fn, false, context)
        .map(|_| ())
        .map_err(|e| anyhow!("Failed to set header(): {e}"))
}

fn create_request_ctx_object(context: &mut Context, request: &ScriptRequest) -> Result<JsValue> {
    let obj = create_js_object(context);
    obj.set(
        js_string!("method"),
        JsValue::from(js_string!(request.method.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.method: {e}"))?;
    obj.set(
        js_string!("path"),
        JsValue::from(js_string!(request.path.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.path: {e}"))?;

    let path_params_obj = create_js_object(context);
    for (k, v) in &request.path_params {
        path_params_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set ctx.request.pathParams.{k}: {e}"))?;
    }
    obj.set(js_string!("pathParams"), path_params_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.pathParams: {e}"))?;

    // A repeated key or header is an array, a single one a string (issue #1629).
    let query_obj = values_object(context, &request.query)
        .map_err(|e| anyhow!("Failed to set ctx.request.query: {e}"))?;
    obj.set(js_string!("query"), query_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.query: {e}"))?;

    let headers_obj = create_js_object(context);
    for (k, v) in &request.headers {
        let value = values_to_js(context, v);
        headers_obj
            .set(js_string!(k.to_ascii_lowercase()), value, false, context)
            .map_err(|e| anyhow!("Failed to set ctx.request.headers.{k}: {e}"))?;
    }
    obj.set(js_string!("headers"), headers_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.headers: {e}"))?;
    set_header_getter(&obj, &request.headers, context)?;

    // ctx.request.body is always the raw string (issue #357 Item 1); fall back to
    // re-serializing the parsed `body` for callers that only populated that field.
    let raw = request.raw_body.clone().unwrap_or_else(|| {
        if request.body.is_null() {
            String::new()
        } else {
            serde_json::to_string(&request.body).unwrap_or_default()
        }
    });
    let json_val = parse_json_or_null(context, &raw)?;
    obj.set(js_string!("json"), json_val, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request.json: {e}"))?;
    obj.set(
        js_string!("body"),
        JsValue::from(js_string!(raw)),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.request.body: {e}"))?;

    Ok(obj.into())
}

fn optional_string_to_js(value: &Option<String>) -> JsValue {
    match value {
        Some(s) => JsValue::from(js_string!(s.clone())),
        None => JsValue::null(),
    }
}

fn create_stub_ctx_object(context: &mut Context, stub: &ScriptStubContext) -> Result<JsValue> {
    let obj = create_js_object(context);
    obj.set(
        js_string!("scenarioName"),
        optional_string_to_js(&stub.scenario_name),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.scenarioName: {e}"))?;
    obj.set(
        js_string!("scenarioState"),
        optional_string_to_js(&stub.scenario_state),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.scenarioState: {e}"))?;
    obj.set(
        js_string!("id"),
        optional_string_to_js(&stub.stub_id),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.stub.id: {e}"))?;
    Ok(obj.into())
}

/// Captures for the `ctx.state` methods: the flow id they're bound to.
type StateCaptures = String;

fn state_get(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    // `ctx.state` is ALWAYS fail-loud (issue #358): a backend failure always raises.
    let outcome = with_current_flow_store(|store| flow_result("get", store.get(flow_id, &key)));
    match outcome {
        Some(Ok(Some(value))) => json_to_js_result(ctx, &value),
        Some(Ok(None)) => Ok(JsValue::null()),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

fn state_set(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let value = args.get(1).cloned().unwrap_or(JsValue::null());
    let json_value = js_to_json(ctx, &value)?;
    let outcome = with_current_flow_store(|store| {
        flow_result("set", store.set(flow_id, &key, json_value).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

fn state_incr(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome =
        with_current_flow_store(|store| flow_result("increment", store.increment(flow_id, &key)));
    js_flow_outcome_strict(outcome)
}

fn state_exists(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome =
        with_current_flow_store(|store| flow_result("exists", store.exists(flow_id, &key)));
    js_flow_outcome_strict(outcome)
}

fn state_delete(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let outcome = with_current_flow_store(|store| {
        flow_result("delete", store.delete(flow_id, &key).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

/// Get a value, or `default` if the key is absent (issue #358) — kills the
/// `state.x = state.x || 0` idiom. A store failure ALWAYS raises (fail-loud), never conflated with
/// "absent" the way `state_get`'s lenient fallback would.
fn state_get_or(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let default = args.get(1).cloned().unwrap_or(JsValue::null());
    let outcome = with_current_flow_store(|store| flow_result("getOr", store.get(flow_id, &key)));
    match outcome {
        Some(Ok(Some(value))) => json_to_js_result(ctx, &value),
        Some(Ok(None)) => Ok(default),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// Atomic increment by `n`, starting at 0 when absent (issue #358). Always fail-loud.
fn state_incr_by(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let by = args
        .get(1)
        .and_then(|v| v.as_number())
        .map(|n| n as i64)
        .ok_or_else(|| JsNativeError::typ().with_message("n must be a number"))?;
    let outcome = with_current_flow_store(|store| {
        flow_result("incrementBy", store.increment_by(flow_id, &key, by))
    });
    js_flow_outcome_strict(outcome)
}

/// Convert a [`CasOutcome`] to the JS return shape for `ctx.state.cas()` (issue #358): an object —
/// `{ applied: true, current: null }` on success, or `{ applied: false, current: <value> }` on
/// conflict — deliberately an object rather than a bare value so "conflict, current value happens
/// to be `true`" can never be confused with "applied".
fn cas_outcome_to_js(context: &mut Context, outcome: CasOutcome) -> JsResult<JsValue> {
    let obj = create_js_object(context);
    let (applied, current_js) = match outcome {
        CasOutcome::Applied => (true, JsValue::null()),
        CasOutcome::Conflict(current) => {
            let current_js = match &current {
                Some(v) => json_to_js_result(context, v)?,
                None => JsValue::null(),
            };
            (false, current_js)
        }
    };
    obj.set(
        js_string!("applied"),
        JsValue::from(applied),
        false,
        context,
    )
    .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    obj.set(js_string!("current"), current_js, false, context)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()))?;
    Ok(obj.into())
}

/// Atomic compare-and-set (issue #358, #311): `key` is set to `new` iff its current value equals
/// `expected` (`null`/`undefined` means "not present"). Always fail-loud.
fn state_cas(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("key must be a string"))?;
    let expected_js = args.get(1).cloned().unwrap_or(JsValue::null());
    let expected_json = js_to_json(ctx, &expected_js)?;
    let expected = if expected_json.is_null() {
        None
    } else {
        Some(expected_json)
    };
    let new_js = args.get(2).cloned().unwrap_or(JsValue::null());
    let new_json = js_to_json(ctx, &new_js)?;

    let outcome = with_current_flow_store(|store| {
        flow_result(
            "cas",
            store.compare_and_set(flow_id, &key, expected.as_ref(), new_json),
        )
    });
    match outcome {
        Some(Ok(cas_outcome)) => cas_outcome_to_js(ctx, cas_outcome),
        Some(Err(msg)) => Err(JsNativeError::error().with_message(msg).into()),
        None => Err(JsNativeError::error()
            .with_message("no flow store bound on this thread")
            .into()),
    }
}

/// TTL override, dispatched on the first argument's type (issue #358, per-key added by #530):
/// `ttl(seconds)` re-stamps every current key of the flow; `ttl(key, seconds)` sets a single key's
/// TTL (returning `true` if it existed, `false` if absent) and `seconds <= 0` deletes it. Always
/// fail-loud. The error names both signatures so the #530 "TypeError on `ttl("k", 1)`" can't recur.
fn state_ttl(
    _this: &JsValue,
    args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    // Per-key form: first arg is a string key, second is the TTL.
    if let Some(key) = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
    {
        let ttl_seconds = args
            .get(1)
            .and_then(|v| v.as_number())
            .map(|n| n as i64)
            .ok_or_else(|| {
                JsNativeError::typ().with_message("ttl(key, seconds): seconds must be a number")
            })?;
        let outcome = with_current_flow_store(|store| {
            flow_result("keyTtl", store.set_key_ttl(flow_id, &key, ttl_seconds))
        });
        return js_flow_outcome_strict(outcome);
    }
    // Flow-level form: first arg is the TTL number.
    if let Some(ttl_seconds) = args.first().and_then(|v| v.as_number()).map(|n| n as i64) {
        let outcome = with_current_flow_store(|store| {
            flow_result("ttl", store.set_ttl(flow_id, ttl_seconds).map(|()| true))
        });
        return js_flow_outcome_strict(outcome);
    }
    Err(JsNativeError::typ()
        .with_message(
            "expected ttl(seconds) or ttl(key, seconds): a number, or a string key and a number",
        )
        .into())
}

/// Remove every key in the flow (issue #530): `ctx.state.clear()`. Always fail-loud.
fn state_clear(
    _this: &JsValue,
    _args: &[JsValue],
    flow_id: &StateCaptures,
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let outcome = with_current_flow_store(|store| {
        flow_result("clear", store.clear_flow(flow_id).map(|()| true))
    });
    js_flow_outcome_strict(outcome)
}

/// `ctx.state` — a flow-state handle bound to one flow id (issue #357 Item 1; atomic ops
/// `getOr`/`incrBy`/`cas`/`ttl` added by issue #358).
type StateMethodFn = fn(&JsValue, &[JsValue], &StateCaptures, &mut Context) -> JsResult<JsValue>;

fn create_state_object(context: &mut Context, flow_id: String) -> Result<JsValue> {
    let obj = create_js_object(context);
    let methods: [(&str, StateMethodFn); 10] = [
        ("get", state_get),
        ("set", state_set),
        ("incr", state_incr),
        ("exists", state_exists),
        ("delete", state_delete),
        ("getOr", state_get_or),
        ("incrBy", state_incr_by),
        ("cas", state_cas),
        ("ttl", state_ttl),
        ("clear", state_clear),
    ];
    for (name, func) in methods {
        let native_fn = NativeFunction::from_copy_closure_with_captures(func, flow_id.clone())
            .to_js_function(context.realm());
        obj.set(js_string!(name), native_fn, false, context)
            .map_err(|e| anyhow!("Failed to set ctx.state.{name}: {e}"))?;
    }
    Ok(obj.into())
}

fn store_flow_method(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let flow_id = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("flow_id must be a string"))?;
    create_state_object(ctx, flow_id)
        .map_err(|e| JsNativeError::error().with_message(e.to_string()).into())
}

/// `ctx.store`: the flow-store escape hatch (issue #357 Item 1) — `.flow(id)` returns a handle
/// scoped to an arbitrary flow id.
fn create_store_object(context: &mut Context) -> Result<JsValue> {
    let obj = create_js_object(context);
    register_method(&obj, "flow", store_flow_method, context)?;
    Ok(obj.into())
}

/// Build the v2 `ctx` object (issue #357 Item 1): identical field names/semantics across engines —
/// see the doc comment on [`ScriptCtxInput`]. `ctx.logger` reuses P1's native logger (issue #355).
fn create_ctx_object(context: &mut Context, input: &ScriptCtxInput) -> Result<JsValue> {
    let obj = create_js_object(context);

    let request_obj = create_request_ctx_object(context, input.request)?;
    obj.set(js_string!("request"), request_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.request: {e}"))?;

    obj.set(
        js_string!("flowId"),
        JsValue::from(js_string!(input.flow_id.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set ctx.flowId: {e}"))?;

    let stub_obj = create_stub_ctx_object(context, &input.stub)?;
    obj.set(js_string!("stub"), stub_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.stub: {e}"))?;

    let state_obj = create_state_object(context, input.flow_id.clone())?;
    obj.set(js_string!("state"), state_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.state: {e}"))?;

    let store_obj = create_store_object(context)?;
    obj.set(js_string!("store"), store_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.store: {e}"))?;

    let logger_obj = create_script_logger_object(context, input.port, input.stub.stub_id.clone())?;
    obj.set(js_string!("logger"), logger_obj, false, context)
        .map_err(|e| anyhow!("Failed to set ctx.logger: {e}"))?;

    Ok(obj.into())
}

// =============================================================================
// v2 result constructors: http()/delay()/reset()/pass() (issue #357 Item 3)
// =============================================================================

fn wrap_script_result(context: &mut Context, result: ScriptResult) -> JsResult<JsValue> {
    let id = SCRIPT_RESULT_NEXT_ID.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    SCRIPT_RESULT_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, result);
    });
    let obj = create_js_object(context);
    obj.set(
        js_string!("__riftResultId"),
        JsValue::from(id as f64),
        false,
        context,
    )?;
    obj.set(
        PropertyKey::from(js_string!("header")),
        NativeFunction::from_fn_ptr(script_result_header).to_js_function(context.realm()),
        false,
        context,
    )?;
    Ok(obj.into())
}

fn script_result_header(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = this
        .as_object()
        .and_then(|o| o.get(js_string!("__riftResultId"), ctx).ok())
        .and_then(|v| v.as_number())
        .map(|n| n as u64)
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header() called on a non-result value")
        })?;
    let key = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header(name, value): name must be a string")
        })?;
    let value = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| {
            JsNativeError::typ().with_message("header(name, value): value must be a string")
        })?;
    SCRIPT_RESULT_REGISTRY.with(|r| {
        if let Some(res) = r.borrow_mut().get_mut(&id) {
            res.add_header(key, value);
        }
    });
    Ok(this.clone())
}

fn js_value_to_script_result_body(ctx: &mut Context, v: &JsValue) -> JsResult<ScriptResultBody> {
    if let Some(s) = v.as_string() {
        Ok(ScriptResultBody::Str(s.to_std_string_escaped()))
    } else {
        Ok(ScriptResultBody::Json(js_to_json(ctx, v)?))
    }
}

fn ctor_http(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let status = args.first().and_then(JsValue::as_number).unwrap_or(200.0);
    let body = match args.get(1) {
        None => None,
        Some(v) if v.is_null() || v.is_undefined() => None,
        Some(v) => Some(js_value_to_script_result_body(ctx, v)?),
    };
    wrap_script_result(ctx, ScriptResult::http(clamp_u16_f64(status), body))
}

fn ctor_delay(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = args.first().and_then(JsValue::as_number).unwrap_or(0.0);
    wrap_script_result(ctx, ScriptResult::Delay(ms.max(0.0) as u64))
}

fn ctor_reset(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    wrap_script_result(ctx, ScriptResult::Reset)
}

fn ctor_pass(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    wrap_script_result(ctx, ScriptResult::Pass)
}

/// Register the v2 result constructors as globals. Cheap and idempotent, so it's called on every
/// script run rather than requiring every `Context` creation site in this module to remember it.
fn register_result_constructors(context: &mut Context) -> Result<()> {
    let global = context.global_object();
    for (name, func) in [
        (
            "http",
            ctor_http as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
        ),
        ("delay", ctor_delay),
        ("reset", ctor_reset),
        ("pass", ctor_pass),
    ] {
        global
            .set(
                js_string!(name),
                NativeFunction::from_fn_ptr(func).to_js_function(context.realm()),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set global {name}: {e}"))?;
    }
    Ok(())
}

/// Convert a `respond(ctx)`/bare-expression return value into a [`FaultDecision`] (issue #357
/// Item 3): `null`/`undefined` (or the script returning nothing) → `None`; an `http()`/`delay()`/
/// `reset()`/`pass()` result → its own outcome; anything else is an error.
fn js_value_to_fault_decision(
    context: &mut Context,
    value: JsValue,
    rule_id: &str,
) -> Result<FaultDecision> {
    if value.is_null() || value.is_undefined() {
        return Ok(FaultDecision::None);
    }
    let id = value
        .as_object()
        .and_then(|o| o.get(js_string!("__riftResultId"), context).ok())
        .and_then(|v| v.as_number())
        .map(|n| n as u64);
    let Some(id) = id else {
        return Err(anyhow!(
            "respond(ctx) must return http(...)/delay(...)/reset()/pass() or nothing"
        ));
    };
    let result = SCRIPT_RESULT_REGISTRY.with(|r| r.borrow_mut().remove(&id));
    let result = result.ok_or_else(|| anyhow!("respond(ctx) result was already consumed"))?;
    Ok(result.into_fault_decision(rule_id))
}

// =============================================================================
// Mountebank v2 `config` calling convention support (issue #355 Item 0)
// =============================================================================

/// Flatten every field of a Mountebank request object onto `config` itself
/// (`config.method`, `config.path`, `config.query`, `config.headers`, `config.body`, and
/// `requestFrom`/`ip`/`form` when present), mirroring
/// Mountebank v2's `downcastInjectionConfig`. This is why `function(config){config.request.path}`,
/// `function(request){request.path}` (with `config` passed positionally where `request` used to
/// be), and `function(config){config.path}` all address the same data.
fn flatten_request_onto(
    config_obj: &JsObject,
    request_obj: &JsValue,
    context: &mut Context,
) -> Result<()> {
    let Some(req) = request_obj.as_object() else {
        return Ok(());
    };
    for key in ["method", "path", "query", "headers", "body"] {
        let val = req
            .get(js_string!(key), context)
            .map_err(|e| anyhow!("Failed to read request.{key} for config flattening: {e}"))?;
        config_obj
            .set(js_string!(key), val, false, context)
            .map_err(|e| anyhow!("Failed to flatten config.{key}: {e}"))?;
    }
    // Issue #1451: the client fields, flattened too, and absent from `config` when absent from
    // the request rather than present as `undefined`.
    for key in ["requestFrom", "ip", "form"] {
        let val = req
            .get(js_string!(key), context)
            .map_err(|e| anyhow!("Failed to read request.{key} for config flattening: {e}"))?;
        if !val.is_undefined() {
            config_obj
                .set(js_string!(key), val, false, context)
                .map_err(|e| anyhow!("Failed to flatten config.{key}: {e}"))?;
        }
    }
    Ok(())
}

// =============================================================================
// Native logger (issue #355 Item 1)
// =============================================================================

/// Log one Mountebank script `logger.<level>(...)` call at target `"rift::script"`, tagging the
/// event with the imposter port and (where available) the stub id. All arguments are best-effort
/// stringified and joined with a space, mirroring a console-style logger call.
fn log_script_event(
    level: tracing::Level,
    port: u16,
    stub_id: Option<&str>,
    args: &[JsValue],
    context: &mut Context,
) {
    let message: String = args
        .iter()
        .map(|v| js_value_to_log_string(context, v))
        .collect::<Vec<_>>()
        .join(" ");
    let stub_id = stub_id.unwrap_or("");
    match level {
        tracing::Level::ERROR => {
            tracing::error!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::WARN => {
            tracing::warn!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::INFO => {
            tracing::info!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::DEBUG => {
            tracing::debug!(target: "rift::script", port, stub_id, "{message}")
        }
        tracing::Level::TRACE => {
            tracing::trace!(target: "rift::script", port, stub_id, "{message}")
        }
    }
}

/// Best-effort stringify of a single logger argument: strings pass through; objects are
/// JSON-stringified; everything else uses JS `display()`.
fn js_value_to_log_string(context: &mut Context, value: &JsValue) -> String {
    if let Some(s) = value.as_string() {
        return s.to_std_string_escaped();
    }
    if value.is_object() {
        let json = js_to_json(context, value).unwrap_or(Value::Null);
        return serde_json::to_string(&json).unwrap_or_default();
    }
    value.display().to_string()
}

/// Captures shared by every native logger method registered on one script's `logger` object:
/// the imposter port and (where the caller has one) the stub id.
type LoggerCaptures = (u16, Option<String>);

fn logger_debug(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::DEBUG,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_info(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::INFO,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_warn(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::WARN,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

fn logger_error(
    _this: &JsValue,
    args: &[JsValue],
    captures: &LoggerCaptures,
    context: &mut Context,
) -> JsResult<JsValue> {
    log_script_event(
        tracing::Level::ERROR,
        captures.0,
        captures.1.as_deref(),
        args,
        context,
    );
    Ok(JsValue::undefined())
}

/// Build a native `logger` object (`debug`/`info`/`warn`/`error`) that routes to `tracing` at
/// target `"rift::script"`, tagged with the imposter port and, where available, the stub id
/// (issue #355 Item 1). A real native object — not a JS no-op shim — so script log calls actually
/// reach the process's tracing subscriber.
/// Function pointer type shared by every native logger method (see [`create_script_logger_object`]).
type LoggerMethodFn = fn(&JsValue, &[JsValue], &LoggerCaptures, &mut Context) -> JsResult<JsValue>;

fn create_script_logger_object(
    context: &mut Context,
    port: u16,
    stub_id: Option<String>,
) -> Result<JsValue> {
    let obj = create_js_object(context);
    let captures: LoggerCaptures = (port, stub_id);

    let methods: [(&str, LoggerMethodFn); 4] = [
        ("debug", logger_debug),
        ("info", logger_info),
        ("warn", logger_warn),
        ("error", logger_error),
    ];

    for (name, func) in methods {
        let native_fn = NativeFunction::from_copy_closure_with_captures(func, captures.clone())
            .to_js_function(context.realm());
        obj.set(js_string!(name), native_fn, false, context)
            .map_err(|e| anyhow!("Failed to set logger.{name}: {e}"))?;
    }

    Ok(obj.into())
}

/// The `console` a Mountebank-style script sees (issue #1593): `log`, `info`, `warn`, `error` and
/// `debug`, routed through the same `tracing` sink as the injected `logger` (`log` at info). In
/// Node `console` writes to the process's stdout; here nothing a script logs is ever printed
/// there.
fn create_console_object(
    context: &mut Context,
    port: u16,
    stub_id: Option<String>,
) -> JsResult<JsObject> {
    let obj = create_js_object(context);
    let captures: LoggerCaptures = (port, stub_id);
    let methods: [(&str, LoggerMethodFn); 5] = [
        ("log", logger_info),
        ("info", logger_info),
        ("warn", logger_warn),
        ("error", logger_error),
        ("debug", logger_debug),
    ];
    for (name, func) in methods {
        let native_fn = NativeFunction::from_copy_closure_with_captures(func, captures.clone())
            .to_js_function(context.realm());
        obj.set(js_string!(name), native_fn, false, context)?;
    }
    Ok(obj)
}

/// Set the host globals every Mountebank-style hook runs with (issue #1593): `console`, and the
/// timers of [`super::js_runtime::install_timers`]. Set afresh for every run, like the other
/// globals the hooks contract on, so a script that overwrote one in the shared realm does not
/// break a later run, and `console` is tagged with the running imposter's port and stub. There is
/// deliberately no `process`, `Buffer` or Node core module: a script has no ambient authority
/// (`docs/mountebank/differences.md`).
fn install_host_globals(context: &mut Context, port: u16, stub_id: Option<&str>) -> Result<()> {
    let console = create_console_object(context, port, stub_id.map(str::to_owned))
        .map_err(|e| anyhow!("Failed to build console: {e}"))?;
    context
        .global_object()
        .set(js_string!("console"), console, false, context)
        .map_err(|e| anyhow!("Failed to set console: {e}"))?;
    super::js_runtime::install_timers(context)
        .map(drop)
        .map_err(|e| anyhow!("Failed to set timers: {e}"))
}

/// Convert JSON Value to JavaScript value
fn json_to_js(context: &mut Context, value: &Value) -> Result<JsValue> {
    json_to_js_result(context, value).map_err(|e| anyhow!("Failed to convert JSON to JS: {e}"))
}

/// Convert JSON Value to JavaScript value (JsResult version)
fn json_to_js_result(context: &mut Context, value: &Value) -> JsResult<JsValue> {
    match value {
        Value::Null => Ok(JsValue::null()),
        Value::Bool(b) => Ok(JsValue::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(JsValue::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(JsValue::from(f))
            } else {
                Ok(JsValue::null())
            }
        }
        Value::String(s) => Ok(JsValue::from(js_string!(s.clone()))),
        Value::Array(arr) => {
            let js_arr = JsArray::new(context)?;
            for (i, v) in arr.iter().enumerate() {
                let js_val = json_to_js_result(context, v)?;
                js_arr.set(i as u32, js_val, false, context)?;
            }
            Ok(js_arr.into())
        }
        Value::Object(obj) => {
            let js_obj = create_js_object(context);
            for (k, v) in obj {
                let js_val = json_to_js_result(context, v)?;
                js_obj.set(js_string!(k.clone()), js_val, false, context)?;
            }
            Ok(js_obj.into())
        }
    }
}

/// How deep a script value may nest when it is read as JSON: a response body, or the state a
/// script leaves (issue #1635). serde_json's own parse limit, so nothing deeper could be posted
/// as an imposter either. Both readers recurse on the native stack — Boa's `JSON.stringify` has
/// no bound of its own — and 1000 levels already overflow a script worker's 2 MiB stack in a
/// debug build.
const MAX_JSON_DEPTH: usize = 128;

/// Convert JavaScript value to JSON Value. A cycle is a `TypeError`, as in `JSON.stringify`; it
/// used to recurse until the stack overflowed and aborted the process.
fn js_to_json(context: &mut Context, value: &JsValue) -> JsResult<Value> {
    js_to_json_within(context, value, &mut Vec::new())
}

/// [`js_to_json`] with the objects being converted above `value`, to detect a cycle.
fn js_to_json_within(
    context: &mut Context,
    value: &JsValue,
    ancestors: &mut Vec<JsObject>,
) -> JsResult<Value> {
    if let Some(obj) = value.as_object() {
        if ancestors.iter().any(|a| JsObject::equals(a, &obj)) {
            return Err(JsNativeError::typ()
                .with_message("cyclic object value")
                .into());
        }
        if ancestors.len() >= MAX_JSON_DEPTH {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "the value nests deeper than {MAX_JSON_DEPTH} levels"
                ))
                .into());
        }
        ancestors.push(obj.clone());
        let converted = js_to_json_value(context, value, ancestors);
        ancestors.pop();
        return converted;
    }
    js_to_json_value(context, value, ancestors)
}

fn js_to_json_value(
    context: &mut Context,
    value: &JsValue,
    ancestors: &mut Vec<JsObject>,
) -> JsResult<Value> {
    if value.is_null() || value.is_undefined() {
        return Ok(Value::Null);
    }

    if let Some(b) = value.as_boolean() {
        return Ok(Value::Bool(b));
    }

    if let Some(n) = value.as_number() {
        // JS has a single number type, so a whole number like `5` arrives as the f64 `5.0`.
        // Encoding it via `from_f64` would store `5.0` (JSON `is_i64() == false`), which the
        // integer-only `increment`/`increment_by` and Redis `INCRBY` can't accumulate — a later
        // `incr` would silently start from 0 (issue #358 B2). So when the value is integral and in
        // i64 range, emit an integer `Number` to keep counters usable across set→incr.
        let number = if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            serde_json::Number::from(n as i64)
        } else {
            serde_json::Number::from_f64(n).unwrap_or(serde_json::Number::from(0))
        };
        return Ok(Value::Number(number));
    }

    if let Some(s) = value.as_string() {
        return Ok(Value::String(s.to_std_string_escaped()));
    }

    if let Some(obj) = value.as_object() {
        // Check if it's an array
        if obj.is_array() {
            let len = obj
                .get(js_string!("length"), context)?
                .as_number()
                .unwrap_or(0.0) as u32;
            let mut arr = Vec::new();
            for i in 0..len {
                let item = obj.get(i, context)?;
                arr.push(js_to_json_within(context, &item, ancestors)?);
            }
            return Ok(Value::Array(arr));
        }

        // It's a regular object
        let mut map = serde_json::Map::new();
        if let Ok(keys) = obj.own_property_keys(context) {
            for key in keys {
                let key_str = match &key {
                    PropertyKey::String(s) => s.to_std_string_escaped(),
                    PropertyKey::Index(i) => i.get().to_string(),
                    PropertyKey::Symbol(_) => continue, // Skip symbols
                };
                if let Ok(val) = obj.get(key.clone(), context) {
                    map.insert(key_str, js_to_json_within(context, &val, ancestors)?);
                }
            }
        }
        return Ok(Value::Object(map));
    }

    Ok(Value::Null)
}

// =============================================================================
// Mountebank-style inject response support
// =============================================================================

/// Response from a Mountebank inject function. A field the function did not return is `None`
/// (issue #1369): the caller completes it from the imposter's `defaultResponse`, or from `200`/no
/// headers/empty when there is none.
#[derive(Debug, Clone)]
pub struct MountebankInjectResponse {
    /// The `statusCode` as the function returned it (issue #1635): a number, or a string that is
    /// read as Node's `writeHead` reads it when the response is written. `None` for a falsy value.
    pub status_code: Option<crate::behaviors::ResponseStatus>,
    pub headers: Option<crate::util::OrderedMap<String, String>>,
    pub body: Option<String>,
}

impl MountebankInjectResponse {
    /// The body text, empty when none was returned.
    #[cfg(test)]
    pub(crate) fn body_text(&self) -> &str {
        self.body.as_deref().unwrap_or_default()
    }
}

/// Execute a Mountebank-style inject function.
///
/// Implements Mountebank v2's `config`-first calling convention (issue #355 Item 0), built like
/// Mountebank's `downcastInjectionConfig`: a `config` object (`{ request, state, logger,
/// callback }`) with every request field ALSO flattened onto `config` itself, so
/// `function(config){config.request.path}`, `function(request){request.path}` (legacy — `config`
/// is passed where `request` used to be, and works because of the flattening), and
/// `function(config){config.path}` all resolve the same data. Legacy positional args follow
/// `config` unchanged, so old scripts keep working:
///
/// - `function(config) { return response; }` (v2)
/// - `function(request) { return response; }` (legacy, `config` stands in for `request`)
/// - `function(request, state) { return response; }`
/// - `function(request, state, logger, callback) { callback(response); }`
///
/// Full call signature: `fn(config, injectState, logger, done, imposterState)`. `injectState`
/// (arg 2) and `imposterState` (arg 5) are the SAME object as `config.state` — the per-imposter
/// state persisted across calls via `get_imposter_state`/`save_imposter_state`, shared with
/// predicate injects and decorate for the same imposter port. `done` (arg 4, aliased as
/// `config.callback`) is the async-style callback. As in Mountebank, the answer is the first of a
/// returned value that is not `undefined` and a `done` call; `done` may come from a promise job or
/// a timer, which run until there is an answer (a returned or passed promise is awaited), within
/// the run's budget (issue #1593).
///
/// Where response is: `{ statusCode: number, headers: object?, body: string }`
///
/// `files` is what the running imposter's scripts may `require()` (issue #1611).
pub fn execute_mountebank_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    scope: ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
) -> Result<MountebankInjectResponse> {
    let input = InjectInput::new(inject_fn, request, scope, stub_id, files);
    let key_scope = input.scope.clone();
    run_blocking(&key_scope, move |thread, reply, claim| {
        start_inject(thread, input, RunBudget::default_now(), reply, claim);
    })
}

/// Run a Mountebank response-`inject` on the MB script pool with a wall-clock deadline
/// (issue #476) — the async counterpart of [`execute_mountebank_inject`], mirroring the
/// `spawn_blocking` + `tokio::time::timeout` shape of `bounded::should_inject_bounded` but
/// submitting straight to the pool (no blocking thread is parked while the script runs). A run
/// that waits on a promise job or a timer holds no worker meanwhile (issue #1678), and is dropped
/// once this future is (its timeout, or a delete cancelling the request, #1643). No abort flag
/// for a CPU-bound script: Boa exposes no per-instruction interrupt, so the loop-iteration cap
/// (issue #327) is what ends it. The deadline is the imposter's `resolve_script_timeout_ms`
/// budget, shared with `_rift.script`.
pub async fn execute_mountebank_inject_bounded(
    inject_fn: String,
    request: MountebankRequest,
    scope: ScriptScope,
    stub_id: Option<String>,
    files: Arc<ScriptFileAccess>,
    timeout: std::time::Duration,
) -> Result<MountebankInjectResponse> {
    // The budget starts at submission, as the caller's timeout does, so a run ends no later than
    // the caller stops waiting for it (issue #1593).
    let budget = RunBudget::starting_now(timeout);
    let key = state_key(&scope);
    let input = InjectInput {
        inject_fn,
        request,
        scope,
        stub_id,
        files,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    submit_mb_job(Some(key), move |thread, claim| {
        start_inject(thread, input, budget, Reply::Async(tx), claim);
    })?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_recv)) => Err(anyhow!("inject task failed (script worker panicked)")),
        Err(_elapsed) => {
            tracing::warn!(
                "Mountebank inject execution timed out after {}ms",
                timeout.as_millis()
            );
            Err(crate::scripting::ScriptTimeoutError {
                hook: "inject",
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }
            .into())
        }
    }
}

/// The wall-clock budget of one hook run (issues #476, #1593): the deadline its job drain must
/// end by, and the timeout it reports when it does not finish in time.
#[derive(Debug, Clone, Copy)]
struct RunBudget {
    deadline: std::time::Instant,
    timeout_ms: u64,
}

impl RunBudget {
    fn starting_now(timeout: std::time::Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + timeout,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// The budget of a caller that sets no timeout of its own:
    /// [`crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS`].
    fn default_now() -> Self {
        Self::starting_now(std::time::Duration::from_millis(
            crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS,
        ))
    }

    fn timed_out(self, hook: &'static str) -> crate::scripting::ScriptTimeoutError {
        crate::scripting::ScriptTimeoutError {
            hook,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// The settled state of a promise, or `None` when `value` is not one.
fn promise_state(value: &JsValue) -> Option<boa_engine::builtins::promise::PromiseState> {
    // Domain-optional: most hooks return a plain object or nothing, which is not a promise.
    value
        .as_object()
        .and_then(|obj| boa_engine::object::builtins::JsPromise::from_object(obj.clone()).ok())
        .map(|promise| promise.state())
}

fn is_pending_promise(value: &JsValue) -> bool {
    matches!(
        promise_state(value),
        Some(boa_engine::builtins::promise::PromiseState::Pending)
    )
}

/// What a response `inject` runs on, owned so it can move to a pool worker.
struct InjectInput {
    inject_fn: String,
    request: MountebankRequest,
    scope: ScriptScope,
    stub_id: Option<String>,
    files: Arc<ScriptFileAccess>,
}

impl InjectInput {
    fn new(
        inject_fn: &str,
        request: &MountebankRequest,
        scope: ScriptScope,
        stub_id: Option<&str>,
        files: &Arc<ScriptFileAccess>,
    ) -> Self {
        Self {
            inject_fn: inject_fn.to_string(),
            request: request.clone(),
            scope,
            stub_id: stub_id.map(str::to_string),
            files: Arc::clone(files),
        }
    }
}

#[cfg(test)]
fn execute_mountebank_inject_in(
    thread: &mut MbJsThread,
    inject_fn: &str,
    request: &MountebankRequest,
    scope: &ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
) -> Result<MountebankInjectResponse> {
    execute_mountebank_inject_within(
        thread,
        inject_fn,
        request,
        scope,
        stub_id,
        files,
        RunBudget::default_now(),
    )
}

/// Run an inject on `thread` to its end, within `budget`.
#[cfg(test)]
fn execute_mountebank_inject_within(
    thread: &mut MbJsThread,
    inject_fn: &str,
    request: &MountebankRequest,
    scope: &ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
    budget: RunBudget,
) -> Result<MountebankInjectResponse> {
    let input = InjectInput::new(inject_fn, request, scope.clone(), stub_id, files);
    run_here(thread, |thread, reply, claim| {
        start_inject(thread, input, budget, reply, claim);
    })
}

/// Start an inject run on `thread`: call the inject function, and admit the run, which answers
/// `reply` now or once a job gives it an answer.
fn start_inject(
    thread: &mut MbJsThread,
    input: InjectInput,
    budget: RunBudget,
    reply: Reply<Result<MountebankInjectResponse>>,
    claim: Claim,
) {
    // Nobody waits any more (issue #1643): the script never runs.
    if reply.abandoned() {
        return;
    }
    if thread.split_from(&input.scope, &input.files) {
        thread
            .deferred
            .push(Box::new(move |thread: &mut MbJsThread| {
                start_inject(thread, input, budget, reply, claim);
            }));
        return;
    }
    let InjectInput {
        inject_fn,
        request,
        scope,
        stub_id,
        files,
    } = input;
    let realm = thread.realm(&files);
    // Mountebank gives every injection `require` (issue #1611); this run's is bound to its own
    // imposter, passed into the hook's scope (#1645), and dies with the run.
    let (run, require) = match MbRun::begin(realm, &files, scope.port, stub_id.as_deref()) {
        Ok(begun) => begun,
        Err(e) => return reply.send(Err(e)),
    };
    let started = {
        let _active = run.activate(&mut realm.context);
        call_inject(
            realm,
            require,
            &inject_fn,
            &request,
            &scope,
            stub_id.as_deref(),
        )
    };
    let (outcome, started) = match started {
        Ok(outcome) => (Some(outcome), Ok(())),
        Err(end) => (None, Err(end)),
    };
    let hook = InjectHook {
        outcome,
        budget,
        reply,
    };
    realm.admit(
        InFlight {
            run,
            deadline: budget.deadline,
            hook: Box::new(hook),
            scope: Some(scope),
            claim,
        },
        started,
    );
}

/// Build the inject's `config` and call the function; the outcome record its answer goes to.
fn call_inject(
    realm: &mut MbRealm,
    require: JsValue,
    inject_fn: &str,
    request: &MountebankRequest,
    scope: &ScriptScope,
    stub_id: Option<&str>,
) -> Result<JsObject, RunEnd> {
    let request_obj = create_mountebank_request_object(&mut realm.context, request)?;
    // The imposter's one live state object (issue #1678), shared with its predicate injects,
    // response injects and decorates (issue #355 Item 0), and with its runs still in flight.
    let state_obj = realm.live_state(scope)?;
    let MbRealm {
        context, scripts, ..
    } = realm;

    let logger_obj =
        create_script_logger_object(&mut *context, scope.port, stub_id.map(str::to_owned))?;

    // Build config = { request, state, logger } and flatten request fields onto config itself.
    let config_obj = create_js_object(&*context);
    config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.request: {e}"))?;
    config_obj
        .set(js_string!("state"), state_obj.clone(), false, &mut *context)
        .map_err(|e| anyhow!("Failed to set config.state: {e}"))?;
    config_obj
        .set(
            js_string!("logger"),
            logger_obj.clone(),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.logger: {e}"))?;
    flatten_request_onto(&config_obj, &request_obj, &mut *context)?;

    // Wrap the inject function to call it with (config, injectState, logger, done, imposterState).
    // `config.callback` aliases `done` so scripts using either the positional callback or the
    // config field convention both work. As in Mountebank, the answer is the first of: a value
    // the function returned that is not `undefined`, or what `done` was first called with — now or
    // later, from a promise job or a timer (issue #1593). The wrapper returns the outcome record
    // the run is answered from.
    let wrapper_script = format!(
        r#"
        (function (require, __config, __injectState, __logger, __imposterState) {{
        var __outcome = {{ called: false, value: null }};
        var __done = function(r) {{
            if (!__outcome.called) {{ __outcome.called = true; __outcome.value = r; }}
        }};
        __config.callback = __done;
        var __injectFn = {inject_fn};
        var __directResult = __injectFn(__config, __injectState, __logger, __done, __imposterState);
        if (__directResult !== undefined) {{
            __done(__directResult);
        }}
        return __outcome;
        }})
        "#
    );

    // injectState and imposterState are the SAME shared, persisted state object as config.state.
    let args = [
        require,
        config_obj.into(),
        state_obj.clone(),
        logger_obj,
        state_obj,
    ];
    let outcome =
        call_mb_wrapper(context, scripts, &wrapper_script, &args).map_err(RunEnd::Threw)?;
    outcome
        .as_object()
        .ok_or_else(|| RunEnd::Failed(anyhow!("the inject wrapper did not return its outcome")))
}

/// An inject run, answered from its wrapper's outcome record.
struct InjectHook {
    /// `None` when its start failed, which ends it at once.
    outcome: Option<JsObject>,
    budget: RunBudget,
    reply: Reply<Result<MountebankInjectResponse>>,
}

impl MbHook for InjectHook {
    fn name(&self) -> &'static str {
        "inject"
    }

    fn abandoned(&self) -> bool {
        self.reply.abandoned()
    }

    /// Whether the run has an answer that is not a pending promise: a returned or called-back
    /// promise is awaited, as Mountebank's `done(promise)` adopts it.
    fn answered(&self, context: &mut Context) -> bool {
        let Some(outcome) = &self.outcome else {
            return false;
        };
        // The outcome is the wrapper's own object, but the inject source is pasted into the
        // wrapper's scope, so a script can reach `__outcome` and give it a getter. That runs only
        // this run's code, in its own realm; a getter that throws reads as no answer.
        let called = outcome
            .get(js_string!("called"), context)
            .is_ok_and(|v| v.to_boolean());
        called
            && !outcome
                .get(js_string!("value"), context)
                .is_ok_and(|v| is_pending_promise(&v))
    }

    fn finish(self: Box<Self>, context: &mut Context, end: RunEnd) {
        let answer = self.answer(context, end);
        self.reply.send(answer);
    }
}

impl InjectHook {
    /// The inject's response for how its run ended. A run whose jobs ended without an answer is
    /// left with none, which `parse_mountebank_response` refuses.
    fn answer(&self, context: &mut Context, end: RunEnd) -> Result<MountebankInjectResponse> {
        let failed = |e: boa_engine::JsError, context: &mut Context| {
            anyhow::Error::new(InjectionFailure {
                detail: format!("Failed to execute inject function: {e}"),
                data: thrown_message(&e, context),
            })
        };
        match end {
            RunEnd::Failed(e) => return Err(e),
            RunEnd::Threw(e) => return Err(failed(e, context)),
            RunEnd::Deadline => return Err(self.budget.timed_out("inject").into()),
            RunEnd::Abandoned => return Err(anyhow!("the inject run was abandoned")),
            RunEnd::Finished | RunEnd::Idle => {}
        }
        let outcome = self
            .outcome
            .as_ref()
            .ok_or_else(|| anyhow!("the inject wrapper did not return its outcome"))?;
        let mut result = outcome
            .get(js_string!("value"), context)
            .map_err(|e| anyhow!("Failed to read the inject outcome: {e}"))?;
        match promise_state(&result) {
            None => {}
            Some(boa_engine::builtins::promise::PromiseState::Fulfilled(value)) => result = value,
            Some(boa_engine::builtins::promise::PromiseState::Rejected(reason)) => {
                let thrown = boa_engine::JsError::from_opaque(reason.clone());
                return Err(anyhow::Error::new(InjectionFailure {
                    detail: format!(
                        "the inject function's promise was rejected: {}",
                        reason.display()
                    ),
                    data: thrown_message(&thrown, context),
                }));
            }
            Some(boa_engine::builtins::promise::PromiseState::Pending) => {
                return Err(anyhow::Error::new(InjectionFailure {
                    detail: "the inject function's promise never settled".to_string(),
                    data: None,
                }));
            }
        }
        parse_mountebank_response(context, result)
    }
}

/// Saves the state a script left as the imposter's script state. A state that cannot be read as a
/// JSON object (a cycle, nesting past [`MAX_JSON_DEPTH`], or no longer an object) keeps the state
/// from before the run, as before, but is logged at error level: Mountebank keeps the live object,
/// so the script's next run would see the change there and does not here (issue #1635).
fn save_script_state(
    context: &mut Context,
    state: &JsValue,
    imposter_state: &mut serde_json::Map<String, Value>,
    scope: &ScriptScope,
    hook: &str,
) {
    match js_to_json(context, state) {
        Ok(Value::Object(map)) => *imposter_state = map,
        Ok(_) => tracing::error!(
            "imposter {}: the {hook}'s state is not saved: it is no longer an object",
            scope.port
        ),
        Err(e) => tracing::error!(
            "imposter {}: the {hook}'s state is not saved: it cannot be stored as \
             JSON: {e}",
            scope.port
        ),
    }
}

/// A Mountebank response `inject` that threw at serve time. `detail` is Rift's message text;
/// `data` is Mountebank's `data` (issue #1581): the thrown value's `message`, kept structured
/// because the handler cannot recover it from the flattened text.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct InjectionFailure {
    pub detail: String,
    /// The thrown value's `message` property as a string; `None` for `throw 'str'`, `throw 42`
    /// or an object without a `message` (Mountebank leaves `data` out then too).
    pub data: Option<String>,
}

/// The `message` of a thrown JS value, or `None` when it is not an object carrying one.
fn thrown_message(e: &boa_engine::JsError, context: &mut Context) -> Option<String> {
    // Unconvertible thrown value: no readable message, so no `data` (domain-optional).
    let thrown = e.clone().into_opaque(context).ok()?;
    let object = thrown.as_object()?;
    // A getter that throws, or a non-string `message` that cannot be stringified, means the
    // message is unreadable: Mountebank would then have no `data` either (domain-optional).
    let message = object.get(js_string!("message"), context).ok()?;
    if message.is_undefined() {
        return None;
    }
    message
        .to_string(context)
        .ok()
        .map(|s| s.to_std_string_escaped())
}

/// A Mountebank predicate `inject` function failed — an object-build failure or the script
/// itself throwing/erroring. Mountebank's own `InjectionError` fails the request loud rather
/// than silently treating the predicate as non-matching (issue #440); this type lets the HTTP
/// handler tell this class of error apart, via `anyhow::Error::downcast_ref`, from other matcher
/// errors (e.g. a scenario-state backend failure, issue #318) that must keep their existing 5xx
/// mapping instead of becoming a 400.
#[derive(Debug, thiserror::Error)]
#[error("invalid predicate injection: {0}")]
pub struct PredicateInjectionError(pub String, pub Option<String>);

impl PredicateInjectionError {
    /// A failure with no thrown `message` to report (an object-build failure, not a throw).
    fn internal(detail: String) -> Self {
        Self(detail, None)
    }

    /// Mountebank's `data` for this failure (issue #1581).
    #[must_use]
    pub fn data(&self) -> Option<&str> {
        self.1.as_deref()
    }
}

/// Execute a Mountebank-style predicate inject function.
///
/// v2 `config`-first calling convention (issue #355 Item 0): full signature
/// `fn(config, logger, imposterState) { return bool; }`, where `config` also stands in for the
/// legacy `request` positional argument (request fields are flattened onto `config`), and
/// `imposterState` aliases `config.state` — the same per-port state shared with response injects
/// and decorate for the same imposter. Returns `Ok(true)`/`Ok(false)` for a legitimate
/// match/non-match. Any failure to build the script's inputs, or the inject function itself
/// throwing/erroring, returns `Err(PredicateInjectionError)` (issue #440) — mirroring Mountebank,
/// which fails the request rather than treating an injection error as "did not match".
pub fn execute_predicate_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    scope: ScriptScope,
    files: &Arc<ScriptFileAccess>,
) -> anyhow::Result<bool> {
    let input = PredicateInput {
        inject_fn: inject_fn.to_string(),
        request: request.clone(),
        scope: scope.clone(),
        files: Arc::clone(files),
    };
    run_blocking(&scope, move |thread, reply, claim| {
        start_predicate_inject(thread, input, reply, claim);
    })
}

/// What a predicate `inject` runs on, owned so it can move to a pool worker.
struct PredicateInput {
    inject_fn: String,
    request: MountebankRequest,
    scope: ScriptScope,
    files: Arc<ScriptFileAccess>,
}

#[cfg(test)]
fn execute_predicate_inject_in(
    thread: &mut MbJsThread,
    inject_fn: &str,
    request: &MountebankRequest,
    scope: &ScriptScope,
    files: &Arc<ScriptFileAccess>,
) -> anyhow::Result<bool> {
    let input = PredicateInput {
        inject_fn: inject_fn.to_string(),
        request: request.clone(),
        scope: scope.clone(),
        files: Arc::clone(files),
    };
    run_here(thread, |thread, reply, claim| {
        start_predicate_inject(thread, input, reply, claim);
    })
}

/// Start a predicate inject run on `thread`. Mountebank does not wait for a predicate: the run
/// answers as soon as its function returns, and the timers and promise jobs it queued end with it
/// (issue #1593).
fn start_predicate_inject(
    thread: &mut MbJsThread,
    input: PredicateInput,
    reply: Reply<anyhow::Result<bool>>,
    claim: Claim,
) {
    if thread.split_from(&input.scope, &input.files) {
        thread
            .deferred
            .push(Box::new(move |thread: &mut MbJsThread| {
                start_predicate_inject(thread, input, reply, claim);
            }));
        return;
    }
    let PredicateInput {
        inject_fn,
        request,
        scope,
        files,
    } = input;
    let realm = thread.realm(&files);
    let (run, require) = match MbRun::begin(realm, &files, scope.port, None) {
        Ok(begun) => begun,
        Err(e) => {
            return reply.send(Err(
                PredicateInjectionError::internal(format!("{e:#}")).into()
            ));
        }
    };
    let started = {
        let _active = run.activate(&mut realm.context);
        call_predicate_inject(realm, require, &inject_fn, &request, &scope)
    };
    let (result, started) = match started {
        Ok(result) => (result, Ok(())),
        Err(end) => (false, Err(end)),
    };
    realm.admit(
        InFlight {
            run,
            deadline: std::time::Instant::now(),
            hook: Box::new(PredicateHook { result, reply }),
            scope: Some(scope),
            claim,
        },
        started,
    );
}

/// Build the predicate inject's `config` and call the function; whether it matched. A failure to
/// build its inputs is a [`PredicateInjectionError`] (issue #440); the function throwing is the
/// run's [`RunEnd::Threw`].
fn call_predicate_inject(
    realm: &mut MbRealm,
    require: JsValue,
    inject_fn: &str,
    request: &MountebankRequest,
    scope: &ScriptScope,
) -> Result<bool, RunEnd> {
    let internal = |what: &str, e: &dyn std::fmt::Display| {
        tracing::warn!("inject predicate: failed to build {what}: {e:#}");
        RunEnd::Failed(
            PredicateInjectionError::internal(format!("failed to build {what}: {e}")).into(),
        )
    };
    let request_obj = create_mountebank_request_object(&mut realm.context, request)
        .map_err(|e| internal("request object", &e))?;
    let state_obj = realm
        .live_state(scope)
        .map_err(|e| internal("state object", &e))?;
    let MbRealm {
        context, scripts, ..
    } = realm;

    // stub_id is left None here: predicate matching operates on a `PredicateOperation` tree and
    // the owning stub's id is not threaded through the matcher without an invasive change to the
    // predicate-matching signatures (issue #355 AC1 note).
    let logger_obj = create_script_logger_object(&mut *context, scope.port, None)
        .map_err(|e| internal("logger object", &e))?;

    let config_obj = create_js_object(&*context);
    if config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut *context,
        )
        .is_err()
        || config_obj
            .set(js_string!("state"), state_obj.clone(), false, &mut *context)
            .is_err()
        || config_obj
            .set(
                js_string!("logger"),
                logger_obj.clone(),
                false,
                &mut *context,
            )
            .is_err()
        || flatten_request_onto(&config_obj, &request_obj, &mut *context).is_err()
    {
        return Err(internal("config object", &"a property could not be set"));
    }

    let wrapper_script = format!(
        r#"
        (function (require, __config, __logger, __imposterState) {{
        var __injectFn = {inject_fn};
        var __result = __injectFn(__config, __logger, __imposterState);
        return Boolean(__result);
        }})
        "#
    );
    let args = [require, config_obj.into(), logger_obj, state_obj];
    let result =
        call_mb_wrapper(context, scripts, &wrapper_script, &args).map_err(RunEnd::Threw)?;
    Ok(result.to_boolean())
}

/// A predicate inject run: answered as soon as it starts.
struct PredicateHook {
    result: bool,
    reply: Reply<anyhow::Result<bool>>,
}

impl MbHook for PredicateHook {
    fn name(&self) -> &'static str {
        "predicate inject"
    }

    fn abandoned(&self) -> bool {
        self.reply.abandoned()
    }

    fn answered(&self, _context: &mut Context) -> bool {
        true
    }

    fn bequeaths(&self) -> bool {
        false
    }

    /// `Ok(true)`/`Ok(false)` for a match/non-match. The function throwing is an `Err`
    /// (issue #440), as Mountebank fails the request rather than treating an injection error as
    /// "did not match".
    fn finish(self: Box<Self>, context: &mut Context, end: RunEnd) {
        let answer = match end {
            RunEnd::Failed(e) => Err(e),
            RunEnd::Threw(e) => {
                tracing::warn!("inject predicate: script execution error: {e}");
                Err(PredicateInjectionError(
                    format!("script execution error: {e}"),
                    thrown_message(&e, context),
                )
                .into())
            }
            RunEnd::Finished | RunEnd::Idle | RunEnd::Deadline | RunEnd::Abandoned => {
                Ok(self.result)
            }
        };
        self.reply.send(answer);
    }
}

/// Execute a `predicateGenerators.inject` function during proxy recording.
///
/// The function receives `(config, logger, predicates)` where `config.request` is the
/// proxied request and `predicates` is the array built so far by `matches`-based generators.
/// It returns an array of predicate objects that are appended to the final predicate list.
pub fn execute_predicate_generator_inject(
    inject_fn: &str,
    request: &MountebankRequest,
    existing_predicates: &[serde_json::Value],
    files: &Arc<ScriptFileAccess>,
) -> Result<Vec<serde_json::Value>, PredicateGeneratorError> {
    let inject_fn = inject_fn.to_string();
    let request = request.clone();
    let existing_predicates = existing_predicates.to_vec();
    let files = Arc::clone(files);
    match with_mb_js_thread(move |thread| {
        execute_predicate_generator_inject_in(
            thread,
            &inject_fn,
            &request,
            &existing_predicates,
            &files,
        )
    }) {
        Ok(result) => result,
        Err(e) => Err(PredicateGeneratorError::Pool(e.to_string())),
    }
}

fn execute_predicate_generator_inject_in(
    thread: &mut MbJsThread,
    inject_fn: &str,
    request: &MountebankRequest,
    existing_predicates: &[serde_json::Value],
    files: &Arc<ScriptFileAccess>,
) -> Result<Vec<serde_json::Value>, PredicateGeneratorError> {
    let realm = thread.realm(files);
    let (run, require) = MbRun::begin(realm, files, 0, None)
        .map_err(|e| PredicateGeneratorError::Script(format!("{e:#}")))?;
    let MbRealm {
        context, scripts, ..
    } = realm;
    let _active = run.activate(context);

    let request_obj = create_mountebank_request_object(&mut *context, request).map_err(|e| {
        PredicateGeneratorError::Script(format!("failed to build request object: {e}"))
    })?;

    let predicates_val = json_to_js(&mut *context, &Value::Array(existing_predicates.to_vec()))
        .map_err(|e| {
            PredicateGeneratorError::Script(format!("failed to build predicates array: {e}"))
        })?;

    // No imposter is running yet during proxy recording, so there is no per-port state to tag
    // the logger with (port 0 placeholder) and no stub id exists yet (stub_id left None).
    let logger_obj = create_script_logger_object(&mut *context, 0, None).map_err(|e| {
        PredicateGeneratorError::Script(format!("failed to build logger object: {e}"))
    })?;

    let wrapper_script = format!(
        r#"
        (function (require, __request, __logger, __predicates) {{
        var __injectFn = {inject_fn};
        var __config = {{ request: __request }};
        var __result = __injectFn(__config, __logger, __predicates);
        return JSON.stringify(__result);
        }})
        "#
    );

    let args = [require, request_obj, logger_obj, predicates_val];
    let result = match call_mb_wrapper(context, scripts, &wrapper_script, &args) {
        Ok(result) => result,
        Err(e) => {
            return Err(PredicateGeneratorError::Threw {
                message: crate::behaviors::thrown_message(&e, context),
                detail: e.to_string(),
            });
        }
    };

    let json_str = result
        .as_string()
        .map(|s| s.to_std_string_lossy())
        .ok_or_else(|| {
            PredicateGeneratorError::Output(
                "function did not return a stringifiable value".to_string(),
            )
        })?;

    serde_json::from_str::<Vec<serde_json::Value>>(&json_str).map_err(|e| {
        PredicateGeneratorError::Output(format!("failed to parse returned predicates: {e}"))
    })
}

/// Request structure for Mountebank inject functions
#[derive(Debug, Clone, Default)]
pub struct MountebankRequest {
    pub method: String,
    pub path: String,
    /// Request order, as Mountebank's `querystring.parse` (issue #1596). A repeated key reaches
    /// the script as an array, a single one as a string (issue #1629).
    pub query: crate::util::MultiValueMap,
    /// Request order, as Mountebank's `headersMap.ofRaw` (issue #1606), every value: a repeated
    /// header is an array (issue #1629).
    pub headers: crate::util::MultiValueMap,
    pub body: Option<String>,
    /// The request's body did not decode (issue #1584): the script sees no `body` key at all, as
    /// Mountebank's `request.body` is `undefined` then. When `false`, `body` is always a string,
    /// `""` for a request without one.
    pub body_undefined: bool,
    /// Whether `body` is UTF-8 text or a base64-encoded binary body (issue #636). Mirrors
    /// `ResponseMode`/`_mode` on the response side so an inject function can tell which it got.
    ///
    /// `None` where the caller genuinely does not know — the decorate and predicate-inject paths
    /// do not thread the classification through. Unknown is represented rather than defaulted to
    /// `Text`, because a body that is base64 while the flag says "text" is worse than no flag: it
    /// invites `if (isBinary) … else JSON.parse(body)` and fails it silently.
    pub mode: Option<ResponseMode>,
    /// `requestFrom` / `ip` / `form` as Mountebank's `httpRequest.transform` builds them (issue
    /// #1451): the same strings the matcher and the journal use (`client_addr.to_string()`,
    /// `client_addr.ip()`). `None` leaves the key absent — a caller with no socket, or a request
    /// without a non-empty urlencoded body for `form`.
    pub request_from: Option<String>,
    pub ip: Option<String>,
    pub form: Option<crate::util::MultiValueMap>,
    /// Set only for the create-time dry run of a `predicateGenerators` inject (issue #1580), which
    /// runs it once over an empty `GET /`. Exposed as `request.isDryRun: true` then and absent
    /// otherwise, as in Mountebank, so a script can skip the dry run.
    pub is_dry_run: bool,
}

/// Create a Mountebank-style request object
/// `request.headers` for a Mountebank-shaped script, read case-insensitively (issue #1368).
///
/// Mountebank hands scripts the names the client sent; Rift's HTTP stack cannot see them (hyper keeps
/// the wire case private), and the listener Title-Cases them. A lookup in any case therefore finds the
/// header, which answers every lookup Mountebank answers whatever spelling the client used. `get`,
/// `has`, `set`, `deleteProperty` and `getOwnPropertyDescriptor` resolve the name, so a write or a
/// delete in another case acts on the stored header rather than creating a hidden duplicate.
/// `ownKeys` is not trapped: `Object.keys`, `for…in`, spread and `JSON.stringify` enumerate the
/// stored names.
fn case_insensitive_headers(target: JsObject, context: &mut Context) -> JsResult<JsObject> {
    use boa_engine::object::builtins::JsProxy;

    /// The target's own key matching `key`: the exact key when it exists, else the first own string
    /// key equal to it ignoring ASCII case, else `key` unchanged (symbols and prototype methods).
    fn resolve(target: &JsObject, key: &JsValue, context: &mut Context) -> JsResult<PropertyKey> {
        let property = key.to_property_key(context)?;
        let PropertyKey::String(name) = &property else {
            return Ok(property);
        };
        if target.has_own_property(property.clone(), context)? {
            return Ok(property);
        }
        let wanted = name.to_std_string_escaped();
        for own in target.own_property_keys(context)? {
            if let PropertyKey::String(candidate) = &own
                && candidate
                    .to_std_string_escaped()
                    .eq_ignore_ascii_case(&wanted)
            {
                return Ok(own);
            }
        }
        Ok(property)
    }

    fn target_of(args: &[JsValue]) -> JsResult<JsObject> {
        args.first().and_then(JsValue::as_object).ok_or_else(|| {
            JsNativeError::typ()
                .with_message("proxy trap without a target")
                .into()
        })
    }

    fn key_of(args: &[JsValue]) -> JsValue {
        args.get(1).cloned().unwrap_or_default()
    }

    fn get(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let target = target_of(args)?;
        let key = resolve(&target, &key_of(args), context)?;
        target.get(key, context)
    }

    fn has(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let target = target_of(args)?;
        let key = resolve(&target, &key_of(args), context)?;
        target.has_property(key, context).map(JsValue::from)
    }

    fn set(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let target = target_of(args)?;
        let key = resolve(&target, &key_of(args), context)?;
        let value = args.get(2).cloned().unwrap_or_default();
        target.set(key, value, false, context).map(JsValue::from)
    }

    fn delete_property(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let target = target_of(args)?;
        let key = resolve(&target, &key_of(args), context)?;
        target
            .delete_property_or_throw(key, context)
            .map(JsValue::from)
    }

    fn get_own_property_descriptor(
        _: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let target = target_of(args)?;
        // A non-extensible target may only report descriptors for keys it has under that exact
        // name (a Proxy invariant), so a frozen or sealed headers object answers exact names only.
        let key = if target.is_extensible(context)? {
            resolve(&target, &key_of(args), context)?
        } else {
            key_of(args).to_property_key(context)?
        };
        let describe = context
            .intrinsics()
            .constructors()
            .object()
            .constructor()
            .get(js_string!("getOwnPropertyDescriptor"), context)?;
        let describe = describe.as_callable().ok_or_else(|| {
            JsNativeError::typ().with_message("Object.getOwnPropertyDescriptor is not callable")
        })?;
        describe.call(
            &JsValue::undefined(),
            &[JsValue::from(target), JsValue::from(key)],
            context,
        )
    }

    let proxy = JsProxy::builder(target)
        .get(get)
        .has(has)
        .set(set)
        .delete_property(delete_property)
        .get_own_property_descriptor(get_own_property_descriptor)
        .build(context)?;
    Ok(proxy.into())
}

/// A field's values as a script sees them (issue #1629): one value is a string, several are an
/// array, as Mountebank's `querystring.parse` and `headersMap.ofRaw` build them.
fn values_to_js(context: &mut Context, values: &[String]) -> JsValue {
    match values {
        [one] => JsValue::from(js_string!(one.as_str())),
        many => JsArray::from_iter(
            many.iter().map(|v| JsValue::from(js_string!(v.as_str()))),
            context,
        )
        .into(),
    }
}

/// A plain object of [`values_to_js`], keys in the map's order.
fn values_object(context: &mut Context, map: &crate::util::MultiValueMap) -> JsResult<JsObject> {
    let obj = create_js_object(context);
    for (k, v) in map {
        let value = values_to_js(context, v);
        obj.set(js_string!(k.as_str()), value, false, context)?;
    }
    Ok(obj)
}

fn create_mountebank_request_object(
    context: &mut Context,
    request: &MountebankRequest,
) -> Result<JsValue> {
    let obj = create_js_object(context);

    // Set method
    obj.set(
        js_string!("method"),
        JsValue::from(js_string!(request.method.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set method: {e}"))?;

    // Set path
    obj.set(
        js_string!("path"),
        JsValue::from(js_string!(request.path.clone())),
        false,
        context,
    )
    .map_err(|e| anyhow!("Failed to set path: {e}"))?;

    // A repeated key or header is an array, a single one a string (issue #1629).
    let query_obj = values_object(context, &request.query)
        .map_err(|e| anyhow!("Failed to set query param: {e}"))?;
    obj.set(js_string!("query"), query_obj, false, context)
        .map_err(|e| anyhow!("Failed to set query: {e}"))?;

    let headers_obj = values_object(context, &request.headers)
        .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    let headers_obj = case_insensitive_headers(headers_obj, context)
        .map_err(|e| anyhow!("Failed to wrap headers: {e}"))?;
    obj.set(js_string!("headers"), headers_obj, false, context)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    // A string, as in Mountebank: `""` for a request without a body (issue #1365). Absent only
    // for a body that did not decode, which Mountebank leaves `undefined` (issue #1584). Users
    // call JSON.parse(request.body) themselves if they want parsed JSON.
    if !request.body_undefined {
        obj.set(
            js_string!("body"),
            JsValue::from(js_string!(request.body.as_deref().unwrap_or(""))),
            false,
            context,
        )
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;
    }

    // Issue #636: `body` is base64 text for a binary request, not the raw bytes (JS strings
    // can't hold arbitrary bytes) — `isBinary` lets an inject function tell the two apart
    // instead of treating base64 as if it were the literal body.
    // Absent, not `false`, when the caller could not classify the body — a script must be able to
    // distinguish "text" from "unknown".
    if let Some(mode) = &request.mode {
        obj.set(
            js_string!("isBinary"),
            JsValue::from(*mode == ResponseMode::Binary),
            false,
            context,
        )
        .map_err(|e| anyhow!("Failed to set isBinary: {e}"))?;
    }

    // Issue #1451: absent rather than `undefined`-valued, as in Mountebank's request object.
    for (key, value) in [("requestFrom", &request.request_from), ("ip", &request.ip)] {
        if let Some(value) = value {
            obj.set(
                js_string!(key),
                JsValue::from(js_string!(value.as_str())),
                false,
                context,
            )
            .map_err(|e| anyhow!("Failed to set {key}: {e}"))?;
        }
    }

    // Absent rather than `false` outside a dry run, as in Mountebank (issue #1580).
    if request.is_dry_run {
        obj.set(js_string!("isDryRun"), JsValue::from(true), false, context)
            .map_err(|e| anyhow!("Failed to set isDryRun: {e}"))?;
    }

    // A plain object: only headers get the case-insensitive lookup, form keys are the client's own.
    if let Some(form) = &request.form {
        let form_obj =
            values_object(context, form).map_err(|e| anyhow!("Failed to set form field: {e}"))?;
        obj.set(js_string!("form"), form_obj, false, context)
            .map_err(|e| anyhow!("Failed to set form: {e}"))?;
    }

    Ok(obj.into())
}

/// Parse Mountebank inject response
fn parse_mountebank_response(
    context: &mut Context,
    result: JsValue,
) -> Result<MountebankInjectResponse> {
    let obj = result
        .as_object()
        .ok_or_else(|| anyhow!("Inject function must return an object"))?;

    // statusCode, headers and body are each optional: an absent one is completed by the caller.
    let status_code = status_from_js(
        obj.get(js_string!("statusCode"), context)
            .map_err(|e| anyhow!("Failed to read statusCode: {e}"))?,
        context,
    )?;
    // A number or boolean header value is sent as JavaScript prints it, as Node's `writeHead`
    // sends it (issue #1635); it used to be dropped.
    let headers = headers_from_js(&obj, context)?;

    let body = obj
        .get(js_string!("body"), context)
        .map_err(|e| anyhow!("Failed to read body: {e}"))?;
    let body = body_from_js(&body, context)?;

    Ok(MountebankInjectResponse {
        status_code,
        headers,
        body,
    })
}

/// A decorate script's `response.statusCode` (issue #1558): the string as written while it is one,
/// as Mountebank hands it over.
fn status_to_js(status: &crate::behaviors::ResponseStatus) -> JsValue {
    match status {
        crate::behaviors::ResponseStatus::Code(code) => JsValue::from(i32::from(*code)),
        crate::behaviors::ResponseStatus::Text(text) => JsValue::from(js_string!(text.as_str())),
    }
}

/// A `statusCode` a script returned or left, as Mountebank hands it to Node's `writeHead` (issues
/// #1558, #1635). `None` for `undefined`, `null`, `false` and `NaN`, which are falsy, so the
/// default status completes them. A string is kept as written. An integer in `0..=65535` is a code
/// (`0` stays falsy for [`crate::imposter::status`]). Any other value — a negative, fractional,
/// out-of-range or infinite number, `true`, an object — is its JavaScript `String(value)`, which
/// the status resolver reads as Node does: `"404.7"` is served as 404, `"-1"` or `"true"` answers
/// 500 `ERR_HTTP_INVALID_STATUS_CODE` naming that text.
fn status_from_js(
    value: JsValue,
    context: &mut Context,
) -> Result<Option<crate::behaviors::ResponseStatus>> {
    use crate::behaviors::ResponseStatus;
    if value.is_undefined() || value.is_null() || value.as_boolean() == Some(false) {
        return Ok(None);
    }
    if let Some(text) = value.as_string() {
        return Ok(Some(ResponseStatus::Text(text.to_std_string_escaped())));
    }
    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return Ok(None);
        }
        if n.fract() == 0.0 && (0.0..=f64::from(u16::MAX)).contains(&n) {
            // An integer within `u16`, just checked, so the cast is exact.
            return Ok(Some(ResponseStatus::Code(n as u16)));
        }
    }
    let text = value
        .to_string(context)
        .map_err(|e| anyhow!("Failed to read statusCode: {e}"))?;
    Ok(Some(ResponseStatus::Text(text.to_std_string_escaped())))
}

/// `module.exports` of every module one hook run has loaded, by resolved path (issue #305): a
/// module required twice in one run is read and evaluated once. GC-traced because it holds
/// `JsValue`s, so it is a `Gc<GcRefCell<..>>` capture of the native `require` function.
type RequireCache =
    boa_engine::gc::Gc<boa_engine::gc::GcRefCell<std::collections::HashMap<String, JsValue>>>;

thread_local! {
    /// The hook run being executed on this thread and what its imposter may `require()` (issue
    /// #1611). Each `require` function carries the id of the run that made it and answers only
    /// while that run is the one executing, so a `require` a script stashed in a global of the
    /// shared realm is dead in every other run, whichever imposter that run belongs to, and after
    /// its own run ended (issue #1678: runs of one realm interleave, so this is switched with
    /// every job, by [`MbRun::activate`]).
    static REQUIRE_RUN: RefCell<Option<(u64, Arc<ScriptFileAccess>)>> = const { RefCell::new(None) };
}

/// One Mountebank-style hook run on a realm (issues #1611, #1645, #1678): its id, which every job
/// it queues carries, the files its `require` may read, the `console` it logs through, and the
/// `tracing` dispatcher its logger events go to (the caller's, which may be thread-scoped).
/// Dropping it drops every job and timer the run still has queued, and forgets its timers' ids.
struct MbRun {
    id: super::js_runtime::RunId,
    files: Arc<ScriptFileAccess>,
    console: JsObject,
    dispatch: tracing::Dispatch,
    jobs: std::rc::Rc<super::js_runtime::MbJobExecutor>,
    timers: super::js_runtime::TimerGlobals,
}

impl MbRun {
    /// Start a run on `realm` for an imposter on `port` (0 when none), with the `require` it is
    /// handed: a fresh one bound to `files`, with a new module cache, whose top-level relative
    /// paths resolve from the root (or, for a trusted door, its base). It is passed into the
    /// hook's scope, never set on the global object (issue #1645). The timer globals are set
    /// again, so a script that overwrote one in the shared realm does not break this run.
    fn begin(
        realm: &mut MbRealm,
        files: &Arc<ScriptFileAccess>,
        port: u16,
        stub_id: Option<&str>,
    ) -> Result<(Self, JsValue)> {
        let context = &mut realm.context;
        let console = create_console_object(context, port, stub_id.map(str::to_owned))
            .map_err(|e| anyhow!("Failed to build console: {e}"))?;
        realm
            .timers
            .install(context)
            .map_err(|e| anyhow!("Failed to set timers: {e}"))?;
        let id = super::js_runtime::RunId::next();
        let base = match files.as_ref() {
            ScriptFileAccess::Confined(root) => root.clone(),
            ScriptFileAccess::Trusted { base } => base.clone(),
            ScriptFileAccess::Refused => std::path::PathBuf::new(),
        };
        let cache: RequireCache = boa_engine::gc::Gc::new(boa_engine::gc::GcRefCell::new(
            std::collections::HashMap::new(),
        ));
        let require = require_function(context, cache, id.raw(), base);
        let run = Self {
            id,
            files: Arc::clone(files),
            console,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            jobs: std::rc::Rc::clone(&realm.jobs),
            timers: realm.timers.clone(),
        };
        Ok((run, require))
    }

    /// Make this the run being executed, until the guard drops: the jobs queued meanwhile belong
    /// to it, its `require` answers, the global `console` is its own, and its logger events go to
    /// its dispatcher. Restores the run that was executing before, which is none except for a run
    /// nested inside another on the same thread. Called before every job of the run, as runs of
    /// one realm interleave (issue #1678).
    fn activate(&self, context: &mut Context) -> ActiveRun {
        let previous_thread_run = super::js_runtime::current_run();
        let previous_run = self.jobs.set_current(Some(self.id));
        let previous_require = REQUIRE_RUN
            .with_borrow_mut(|run| run.replace((self.id.raw(), Arc::clone(&self.files))));
        let active = ActiveRun {
            jobs: std::rc::Rc::clone(&self.jobs),
            previous_run,
            previous_thread_run,
            previous_require,
            _dispatch: tracing::dispatcher::set_default(&self.dispatch),
        };
        // Domain-optional: only a script of this realm can make `console` refuse the write (made
        // read-only, or an accessor that throws), and it then keeps its own `console` in its own
        // realm, as an overwritten `console` stays overwritten in Mountebank's one process.
        match context.global_object().set(
            js_string!("console"),
            self.console.clone(),
            false,
            context,
        ) {
            Ok(true) => {}
            Ok(false) => tracing::debug!(
                target: "rift::script",
                "a script made console read-only; this run logs through its console"
            ),
            Err(e) => tracing::debug!(
                target: "rift::script",
                "a script's console refused to be set: {e}"
            ),
        }
        active
    }
}

impl Drop for MbRun {
    fn drop(&mut self) {
        self.jobs.drop_run(self.id);
        self.timers.forget_run(self.id);
    }
}

/// A run being executed ([`MbRun::activate`]); dropping it restores the one executing before.
struct ActiveRun {
    jobs: std::rc::Rc<super::js_runtime::MbJobExecutor>,
    previous_run: Option<super::js_runtime::RunId>,
    /// The run executing on this thread before, which may belong to another realm's executor (a
    /// run nested on a fresh worker state).
    previous_thread_run: Option<super::js_runtime::RunId>,
    previous_require: Option<(u64, Arc<ScriptFileAccess>)>,
    _dispatch: tracing::dispatcher::DefaultGuard,
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.jobs.set_current(self.previous_run);
        super::js_runtime::set_current_run(self.previous_thread_run);
        let previous = self.previous_require.take();
        REQUIRE_RUN.with_borrow_mut(|run| *run = previous);
    }
}

/// A `require` function for run `run`, resolving a relative path from `base`. The captures are all
/// GC-traceable, so `from_copy_closure_with_captures` registers them without `unsafe`: the cache
/// holds `JsValue`s and must stay reachable to the collector.
fn require_function(
    context: &mut Context,
    cache: RequireCache,
    run: u64,
    base: std::path::PathBuf,
) -> JsValue {
    NativeFunction::from_copy_closure_with_captures(
        |_this, args, (cache, run, base): &(RequireCache, u64, std::path::PathBuf), context| {
            require_impl(cache, *run, base, args, context)
        },
        (cache, run, base),
    )
    .to_js_function(context.realm())
    .into()
}

/// A JavaScript `Error` with `message`.
fn require_error(message: String) -> boa_engine::JsError {
    JsNativeError::error().with_message(message).into()
}

/// Where `request` resolves for this run, or the error `require` throws. Under
/// [`ScriptFileAccess::Confined`] a path outside the root gets one fixed message, without a look at
/// the filesystem outside it, so a refusal never says whether anything is there (issue #1611).
fn resolve_module(
    files: &ScriptFileAccess,
    base: &std::path::Path,
    request: &str,
) -> JsResult<std::path::PathBuf> {
    match files {
        ScriptFileAccess::Refused => Err(require_error(format!(
            "require('{request}'): no --scripts-dir is configured"
        ))),
        ScriptFileAccess::Confined(root) => crate::imposter::resolve_confined(root, base, request)
            .map_err(|e| {
                require_error(match e {
                    crate::imposter::ConfinedPathError::Outside => {
                        format!("require('{request}'): outside --scripts-dir")
                    }
                    crate::imposter::ConfinedPathError::NotFound => {
                        format!("require('{request}'): no such module under --scripts-dir")
                    }
                    crate::imposter::ConfinedPathError::Root(e) => {
                        format!("require('{request}'): --scripts-dir cannot be resolved: {e}")
                    }
                    crate::imposter::ConfinedPathError::Read(e) => {
                        format!("require('{request}'): {e}")
                    }
                })
            }),
        ScriptFileAccess::Trusted { .. } => Ok(base.join(request)),
    }
}

/// Minimal CommonJS module loader backing `require()` (issues #305, #1611).
///
/// Resolves `request` under the run's [`ScriptFileAccess`] (relative to `base`: the root, or the
/// requiring module's own directory), reads it, and evaluates it as
/// `(function(module, exports, require, __filename, __dirname) { ... })`. The `require` a module
/// gets resolves relative paths from that module's directory, as Node's does. A `.json` file is
/// parsed rather than evaluated, as in Node. No built-in (`fs`, `path`) and no bare package name
/// resolves: only files.
fn require_impl(
    cache: &RequireCache,
    run: u64,
    base: &std::path::Path,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let request = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .ok_or_else(|| JsNativeError::typ().with_message("require() path must be a string"))?;
    let files = REQUIRE_RUN
        .with_borrow(|current| match current {
            Some((id, files)) if *id == run => Some(Arc::clone(files)),
            _ => None,
        })
        .ok_or_else(|| {
            require_error(format!(
                "require('{request}'): this require belongs to a script run that has ended, or to \
                 another run"
            ))
        })?;

    let resolved_path = resolve_module(&files, base, &request)?;
    let confined = !matches!(files.as_ref(), ScriptFileAccess::Trusted { .. });

    // A confined path is canonical already. A trusted one is keyed by its canonical form when it
    // has one: a domain-optional lookup, since a missing file is reported by the read below.
    let cache_key = if confined {
        resolved_path.to_string_lossy().into_owned()
    } else {
        std::fs::canonicalize(&resolved_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| resolved_path.to_string_lossy().into_owned())
    };
    if let Some(cached) = cache.borrow().get(&cache_key) {
        return Ok(cached.clone());
    }

    // A confined read re-checks the path once the file is open, so a link swapped in after the
    // resolution above cannot reach outside the root (issue #1617).
    let source = match files.as_ref() {
        ScriptFileAccess::Confined(root) => {
            crate::imposter::read_confined(root, base, &request, &resolved_path).map_err(|e| {
                require_error(match e {
                    crate::imposter::ConfinedPathError::Read(e) => {
                        format!("require('{request}'): {e}")
                    }
                    _ => format!("require('{request}'): outside --scripts-dir"),
                })
            })?
        }
        _ => std::fs::read_to_string(&resolved_path)
            .map_err(|e| require_error(format!("require('{request}'): {e}")))?,
    };

    if resolved_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        let parsed: Value = serde_json::from_str(&source)
            .map_err(|e| require_error(format!("require('{request}'): invalid JSON: {e}")))?;
        let exports = json_to_js(context, &parsed)
            .map_err(|e| require_error(format!("require('{request}'): {e}")))?;
        cache.borrow_mut().insert(cache_key, exports.clone());
        return Ok(exports);
    }

    let wrapped_source =
        format!("(function(module, exports, require, __filename, __dirname) {{\n{source}\n}})");
    let wrapper = context.eval(Source::from_bytes(wrapped_source.as_bytes()))?;
    let wrapper_obj = wrapper.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message(format!(
            "require('{request}'): module wrapper is not callable"
        ))
    })?;

    let module_obj = create_js_object(context);
    let exports_obj = create_js_object(context);
    module_obj.set(
        js_string!("exports"),
        JsValue::from(exports_obj.clone()),
        false,
        context,
    )?;

    // Publish the (still-empty) exports to the cache BEFORE evaluating the module, so a circular
    // require() returns the partial module instead of recursing until the native stack overflows
    // (Node's cycle-breaking contract). The final value is re-published after eval below.
    cache
        .borrow_mut()
        .insert(cache_key.clone(), JsValue::from(exports_obj.clone()));

    let dir = resolved_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let filename = resolved_path.to_string_lossy().into_owned();
    let dirname = dir.to_string_lossy().into_owned();
    let module_require = require_function(context, cache.clone(), run, dir);

    wrapper_obj.call(
        &JsValue::undefined(),
        &[
            JsValue::from(module_obj.clone()),
            JsValue::from(exports_obj),
            module_require,
            JsValue::from(js_string!(filename)),
            JsValue::from(js_string!(dirname)),
        ],
        context,
    )?;

    // A module can reassign `module.exports` (e.g. `module.exports = function ...`); publish the
    // final value so later require()s of the same module get the real exports, not the placeholder.
    let module_exports = module_obj.get(js_string!("exports"), context)?;
    cache.borrow_mut().insert(cache_key, module_exports.clone());

    Ok(module_exports)
}

/// Execute a Mountebank `decorate` in Boa, on Mountebank's contract (issue #1567).
///
/// Mountebank calls every decorate as `(fn)(config, response, logger)` (`behaviors.js:239`), so
/// the function may be written in any form (`function`, an arrow, `async`) with any parameter
/// names. `config` carries `request`, `response`, `path`, `state` and `logger`, with every request
/// field also flattened onto it (issue #355 Item 0), and `response` is the same object as
/// `config.response`. A fourth `state` argument is a Rift superset. `state` is the per-port state
/// persisted via `get_imposter_state`/`save_imposter_state`, shared with predicate injects and
/// response injects for the same imposter. A CommonJS `require()` lets a decorate load an external
/// `.cjs`/`.js` module (issue #305), within what `files` allows (issue #1611).
///
/// A truthy return value replaces the response, as in Mountebank (`behaviors.js:246-249`); an
/// `async` function's promise is settled first. Otherwise the result is `config.response` as the
/// function left it. Either way the headers are exactly that object's own, so a deleted header is
/// gone.
#[allow(clippy::too_many_arguments)]
pub fn execute_mountebank_decorate(
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: &crate::behaviors::ResponseStatus,
    response_headers: &crate::util::OrderedMap<String, String>,
    scope: &ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
) -> Result<crate::behaviors::Decorated> {
    execute_mountebank_decorate_within(
        decorate_fn,
        request,
        response_body,
        response_status,
        response_headers,
        scope.clone(),
        stub_id,
        files,
        std::time::Duration::from_millis(crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS),
    )
}

/// [`execute_mountebank_decorate`] with the caller's script timeout (issue #1593): an `async`
/// decorate's jobs and timers run until its promise settles, or until `timeout` from now, when
/// the run fails with a [`crate::scripting::ScriptTimeoutError`]. Blocks until it answers; the
/// serving path awaits [`execute_mountebank_decorate_bounded`] instead.
#[allow(clippy::too_many_arguments)]
pub fn execute_mountebank_decorate_within(
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: &crate::behaviors::ResponseStatus,
    response_headers: &crate::util::OrderedMap<String, String>,
    scope: ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
    timeout: std::time::Duration,
) -> Result<crate::behaviors::Decorated> {
    let budget = RunBudget::starting_now(timeout);
    let input = DecorateInput {
        decorate_fn: decorate_fn.to_string(),
        request: request.clone(),
        response_body: response_body.to_string(),
        response_status: response_status.clone(),
        response_headers: response_headers.clone(),
        scope: scope.clone(),
        stub_id: stub_id.map(str::to_string),
        files: Arc::clone(files),
    };
    run_blocking(&scope, move |thread, reply, claim| {
        start_decorate(thread, input, budget, reply, claim);
    })
}

/// [`execute_mountebank_decorate_within`] for an async caller (issue #1678): no thread waits while
/// an `async` decorate's jobs and timers run, and the run is dropped once this future is. Fails
/// with a [`crate::scripting::ScriptTimeoutError`] when it has not answered within `timeout`.
#[allow(clippy::too_many_arguments)]
pub async fn execute_mountebank_decorate_bounded(
    decorate_fn: String,
    request: MountebankRequest,
    response_body: String,
    response_status: crate::behaviors::ResponseStatus,
    response_headers: crate::util::OrderedMap<String, String>,
    scope: ScriptScope,
    stub_id: Option<String>,
    files: Arc<ScriptFileAccess>,
    timeout: std::time::Duration,
) -> Result<crate::behaviors::Decorated> {
    let budget = RunBudget::starting_now(timeout);
    let key = state_key(&scope);
    let input = DecorateInput {
        decorate_fn,
        request,
        response_body,
        response_status,
        response_headers,
        scope,
        stub_id,
        files,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    submit_mb_job(Some(key), move |thread, claim| {
        start_decorate(thread, input, budget, Reply::Async(tx), claim);
    })?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_recv)) => Err(anyhow!("decorate task failed (script worker panicked)")),
        Err(_elapsed) => Err(budget.timed_out("decorate").into()),
    }
}

/// What a `decorate` runs on, owned so it can move to a pool worker.
struct DecorateInput {
    decorate_fn: String,
    request: MountebankRequest,
    response_body: String,
    response_status: crate::behaviors::ResponseStatus,
    response_headers: crate::util::OrderedMap<String, String>,
    scope: ScriptScope,
    stub_id: Option<String>,
    files: Arc<ScriptFileAccess>,
}

/// Run a decorate on `thread` to its end, with the default budget.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn execute_mountebank_decorate_in(
    thread: &mut MbJsThread,
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: &crate::behaviors::ResponseStatus,
    response_headers: &crate::util::OrderedMap<String, String>,
    scope: &ScriptScope,
    stub_id: Option<&str>,
    files: &Arc<ScriptFileAccess>,
) -> Result<crate::behaviors::Decorated> {
    let input = DecorateInput {
        decorate_fn: decorate_fn.to_string(),
        request: request.clone(),
        response_body: response_body.to_string(),
        response_status: response_status.clone(),
        response_headers: response_headers.clone(),
        scope: scope.clone(),
        stub_id: stub_id.map(str::to_string),
        files: Arc::clone(files),
    };
    run_here(thread, |thread, reply, claim| {
        start_decorate(thread, input, RunBudget::default_now(), reply, claim);
    })
}

/// Start a decorate run on `thread`. A decorate that returns a promise is answered once it
/// settles, its jobs and timers running meanwhile; any other one at once, with no job run, as
/// Mountebank does not wait for a decorate that returns no promise (issue #1593).
fn start_decorate(
    thread: &mut MbJsThread,
    input: DecorateInput,
    budget: RunBudget,
    reply: Reply<Result<crate::behaviors::Decorated>>,
    claim: Claim,
) {
    // Nobody waits any more (issue #1643): the script never runs.
    if reply.abandoned() {
        return;
    }
    if thread.split_from(&input.scope, &input.files) {
        thread
            .deferred
            .push(Box::new(move |thread: &mut MbJsThread| {
                start_decorate(thread, input, budget, reply, claim);
            }));
        return;
    }
    let DecorateInput {
        decorate_fn,
        request,
        response_body,
        response_status,
        response_headers,
        scope,
        stub_id,
        files,
    } = input;
    let realm = thread.realm(&files);
    // A fresh CommonJS `require()` bound to this imposter (issues #305, #1611), passed into the
    // hook's scope (#1645) and live until the run ends.
    let (run, require) = match MbRun::begin(realm, &files, scope.port, stub_id.as_deref()) {
        Ok(begun) => begun,
        Err(e) => return reply.send(Err(e)),
    };
    let started = {
        let _active = run.activate(&mut realm.context);
        call_decorate(
            realm,
            require,
            &decorate_fn,
            &request,
            &response_body,
            &response_status,
            &response_headers,
            &scope,
            stub_id.as_deref(),
        )
    };
    let (called, started) = match started {
        Ok(called) => (Some(called), Ok(())),
        Err(end) => (None, Err(end)),
    };
    let hook = DecorateHook {
        called,
        response_headers,
        budget,
        reply,
    };
    realm.admit(
        InFlight {
            run,
            deadline: budget.deadline,
            hook: Box::new(hook),
            scope: Some(scope),
            claim,
        },
        started,
    );
}

/// A decorate function that has been called: what it returned, and the objects it was handed.
struct DecorateCall {
    returned: JsValue,
    config: JsObject,
    response: JsObject,
}

/// Build the decorate's `config` and `response` and call the function.
#[allow(clippy::too_many_arguments)]
fn call_decorate(
    realm: &mut MbRealm,
    require: JsValue,
    decorate_fn: &str,
    request: &MountebankRequest,
    response_body: &str,
    response_status: &crate::behaviors::ResponseStatus,
    response_headers: &crate::util::OrderedMap<String, String>,
    scope: &ScriptScope,
    stub_id: Option<&str>,
) -> Result<DecorateCall, RunEnd> {
    let request_obj = create_mountebank_request_object(&mut realm.context, request)?;
    let state_obj = realm.live_state(scope)?;
    let MbRealm {
        context, scripts, ..
    } = realm;

    let response_obj = create_js_object(&*context);
    response_obj
        .set(
            js_string!("statusCode"),
            status_to_js(response_status),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set statusCode: {e}"))?;
    response_obj
        .set(
            js_string!("body"),
            JsValue::from(js_string!(response_body.to_string())),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set body: {e}"))?;
    let headers_obj = create_js_object(&*context);
    for (k, v) in response_headers {
        headers_obj
            .set(
                js_string!(k.clone()),
                JsValue::from(js_string!(v.clone())),
                false,
                &mut *context,
            )
            .map_err(|e| anyhow!("Failed to set header: {e}"))?;
    }
    response_obj
        .set(js_string!("headers"), headers_obj, false, &mut *context)
        .map_err(|e| anyhow!("Failed to set headers: {e}"))?;

    let logger_obj =
        create_script_logger_object(&mut *context, scope.port, stub_id.map(str::to_owned))?;

    // config: { request, response, path, state, logger } + the flattened request.
    let config_obj = create_js_object(&*context);
    config_obj
        .set(
            js_string!("request"),
            request_obj.clone(),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.request: {e}"))?;
    config_obj
        .set(
            js_string!("response"),
            JsValue::from(response_obj.clone()),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.response: {e}"))?;
    config_obj
        .set(
            js_string!("path"),
            JsValue::from(js_string!(request.path.clone())),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.path: {e}"))?;
    config_obj
        .set(js_string!("state"), state_obj.clone(), false, &mut *context)
        .map_err(|e| anyhow!("Failed to set config.state: {e}"))?;
    config_obj
        .set(
            js_string!("logger"),
            logger_obj.clone(),
            false,
            &mut *context,
        )
        .map_err(|e| anyhow!("Failed to set config.logger: {e}"))?;
    flatten_request_onto(&config_obj, &request_obj, &mut *context)?;

    // The script's completion value is what the function returned.
    let wrapper_script = format!(
        r#"
        (function (require, __config, __response, __logger, __state) {{
        var __decorateFn = {decorate_fn};
        return __decorateFn(__config, __response, __logger, __state);
        }})
        "#
    );
    let args = [
        require,
        config_obj.clone().into(),
        response_obj.clone().into(),
        logger_obj,
        state_obj,
    ];
    let returned = call_mb_wrapper(context, scripts, &wrapper_script, &args)
        .map_err(|e| anyhow!("Failed to execute decorate function: {e}"))?;
    Ok(DecorateCall {
        returned,
        config: config_obj,
        response: response_obj,
    })
}

/// A decorate run, answered once what its function returned is no pending promise.
struct DecorateHook {
    /// `None` when its start failed, which ends it at once.
    called: Option<DecorateCall>,
    response_headers: crate::util::OrderedMap<String, String>,
    budget: RunBudget,
    reply: Reply<Result<crate::behaviors::Decorated>>,
}

impl MbHook for DecorateHook {
    fn name(&self) -> &'static str {
        "decorate"
    }

    fn abandoned(&self) -> bool {
        self.reply.abandoned()
    }

    fn answered(&self, _context: &mut Context) -> bool {
        self.called
            .as_ref()
            .is_some_and(|called| !is_pending_promise(&called.returned))
    }

    fn finish(self: Box<Self>, context: &mut Context, end: RunEnd) {
        let answer = self.answer(context, end);
        self.reply.send(answer);
    }
}

impl DecorateHook {
    fn answer(&self, context: &mut Context, end: RunEnd) -> Result<crate::behaviors::Decorated> {
        match end {
            RunEnd::Failed(e) => return Err(e),
            RunEnd::Threw(e) => {
                return Err(anyhow!("Failed to run the decorate's promise jobs: {e}"));
            }
            RunEnd::Deadline => return Err(self.budget.timed_out("decorate").into()),
            RunEnd::Abandoned => return Err(anyhow!("the decorate run was abandoned")),
            RunEnd::Finished | RunEnd::Idle => {}
        }
        let called = self
            .called
            .as_ref()
            .ok_or_else(|| anyhow!("the decorate function was not called"))?;
        let returned = match promise_state(&called.returned) {
            None => called.returned.clone(),
            Some(boa_engine::builtins::promise::PromiseState::Fulfilled(value)) => value,
            Some(boa_engine::builtins::promise::PromiseState::Rejected(reason)) => {
                return Err(anyhow!(
                    "the decorate function's promise was rejected: {}",
                    reason.display()
                ));
            }
            Some(boa_engine::builtins::promise::PromiseState::Pending) => {
                return Err(anyhow!("the decorate function's promise never settled"));
            }
        };

        let left = called
            .config
            .get(js_string!("response"), context)
            .map_err(|e| anyhow!("Failed to read config.response: {e}"))?;
        // `return response;` hands back the response it was given: that is a mutation, and keeps
        // what the script never saw (the held-aside Set-Cookie lines).
        let is_given = |value: &JsValue| {
            value.as_object().is_some_and(|obj| {
                JsObject::equals(&obj, &called.response)
                    || left
                        .as_object()
                        .is_some_and(|left| JsObject::equals(&obj, &left))
            })
        };
        if returned.to_boolean() && !is_given(&returned) {
            let obj = returned.as_object().ok_or_else(|| {
                anyhow!(
                    "a decorate function must return a response object or nothing, not {}",
                    returned.display()
                )
            })?;
            return returned_response(&obj, context);
        }

        let obj = left.as_object().ok_or_else(|| {
            anyhow!("a decorate function must leave config.response as an object")
        })?;
        let status_code = status_from_js(
            obj.get(js_string!("statusCode"), context)
                .map_err(|e| anyhow!("Failed to read statusCode: {e}"))?,
            context,
        )?
        // A falsy status the script left (`null`, `NaN`, deleted) is falsy to `postProcess` too,
        // which completes it from `defaultResponse`, then 200 (issue #1635), rather than keeping
        // the old one.
        .unwrap_or(crate::behaviors::ResponseStatus::Code(0));
        let headers =
            headers_from_js(&obj, context)?.unwrap_or_else(|| self.response_headers.clone());
        let body = obj
            .get(js_string!("body"), context)
            .map_err(|e| anyhow!("Failed to read body: {e}"))?;
        Ok(crate::behaviors::Decorated {
            status_code,
            headers,
            body: body_from_js(&body, context)?.unwrap_or_default(),
            returned: None,
        })
    }
}

/// A response object a decorate returned, which replaces the response (issue #1567). A field it
/// does not carry (absent, `undefined` or `null`) is marked omitted, for the pipeline to complete
/// from `defaultResponse`.
fn returned_response(obj: &JsObject, context: &mut Context) -> Result<crate::behaviors::Decorated> {
    let status = status_from_js(
        obj.get(js_string!("statusCode"), context)
            .map_err(|e| anyhow!("Failed to read statusCode: {e}"))?,
        context,
    )?;
    let headers = headers_from_js(obj, context)?;
    let body = obj
        .get(js_string!("body"), context)
        .map_err(|e| anyhow!("Failed to read body: {e}"))?;
    let body = body_from_js(&body, context)?;
    let omits = crate::behaviors::ReturnedOmits {
        status_code: status.is_none(),
        headers: headers.is_none(),
        body: body.is_none(),
    };
    Ok(crate::behaviors::Decorated {
        status_code: status.unwrap_or(crate::behaviors::ResponseStatus::Code(200)),
        headers: headers.unwrap_or_default(),
        body: body.unwrap_or_default(),
        returned: Some(omits),
    })
}

/// A response object's headers: its own string-keyed properties. `None` when `headers` is not an
/// object. A value is a header line when it is a string, number or boolean; any other value is
/// not one line in the single-value model (issue #238) and is dropped with a warning.
fn headers_from_js(
    obj: &JsObject,
    context: &mut Context,
) -> Result<Option<crate::util::OrderedMap<String, String>>> {
    let value = obj
        .get(js_string!("headers"), context)
        .map_err(|e| anyhow!("Failed to read headers: {e}"))?;
    let Some(headers_obj) = value.as_object() else {
        return Ok(None);
    };
    let keys = headers_obj
        .own_property_keys(context)
        .map_err(|e| anyhow!("Failed to list headers: {e}"))?;
    let mut headers = crate::util::OrderedMap::default();
    for key in keys {
        let name = match &key {
            PropertyKey::String(s) => s.to_std_string_escaped(),
            PropertyKey::Index(i) => i.get().to_string(),
            PropertyKey::Symbol(_) => continue,
        };
        let value = headers_obj
            .get(key.clone(), context)
            .map_err(|e| anyhow!("Failed to read header {name}: {e}"))?;
        if let Some(text) = value.as_string() {
            headers.insert(name, text.to_std_string_escaped());
        } else if value.is_number() || value.is_boolean() {
            headers.insert(name, value.display().to_string());
        } else {
            tracing::warn!(
                "script response header {name:?} is {}, not a single header value; it is not sent",
                value.type_of()
            );
        }
    }
    Ok(Some(headers))
}

/// `JSON.stringify(value)` with a replacer that refuses a value nested deeper than `limit`: Boa's
/// `JSON.stringify` recurses on the native stack with no bound of its own. The replacer is called
/// on every value before it is descended into, with the holder as `this`, so the holders above it
/// are the stack, trimmed back to the current holder. It uses no prototype method a script could
/// have replaced, and `stringify` is the intrinsic, passed in.
const BOUNDED_STRINGIFY: &str = r#"(function (stringify, value, limit) {
    var stack = [];
    return stringify(value, function (key, v) {
        while (stack.length > 0 && stack[stack.length - 1] !== this) { stack.length = stack.length - 1; }
        if (v !== null && typeof v === 'object') {
            if (stack.length >= limit) { throw new TypeError('the response body nests deeper than ' + limit + ' levels'); }
            stack[stack.length] = v;
        }
        return v;
    });
})"#;

/// A response body as served: a string as it is, an object as `JSON.stringify` writes it (compact,
/// in the object's key order, a lone surrogate escaped as `\udXXX`, as Node writes it), anything
/// else as JavaScript prints it. `None` for `undefined` or `null`. An object `JSON.stringify`
/// refuses (a cycle, a BigInt) or one nested past [`MAX_JSON_DEPTH`] is an error, as Mountebank's
/// `postProcess` throws on the first two and answers 500.
fn body_from_js(value: &JsValue, context: &mut Context) -> Result<Option<String>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if let Some(s) = value.as_string() {
        return Ok(Some(s.to_std_string_escaped()));
    }
    if !value.is_object() {
        return Ok(Some(value.display().to_string()));
    }
    let stringify = context
        .intrinsics()
        .objects()
        .json()
        .get(js_string!("stringify"), context)
        .map_err(|e| anyhow!("Failed to read JSON.stringify: {e}"))?;
    let bounded = context
        .eval(Source::from_bytes(BOUNDED_STRINGIFY))
        .map_err(|e| anyhow!("Failed to compile the body serializer: {e}"))?;
    let text = bounded
        .as_callable()
        .ok_or_else(|| anyhow!("the body serializer is not a function"))?
        .call(
            &JsValue::undefined(),
            &[
                stringify,
                value.clone(),
                JsValue::from(MAX_JSON_DEPTH as u32),
            ],
            context,
        )
        .map_err(|e| anyhow!("the response body cannot be serialized as JSON: {e}"))?;
    // `undefined` when the object's `toJSON` gives nothing; Mountebank then fails writing it.
    let text = text
        .as_string()
        .ok_or_else(|| anyhow!("the response body serializes to {}", text.display()))?;
    // Well-formed `JSON.stringify` output is ASCII-escaped wherever it is not valid UTF-16, so
    // this conversion is exact.
    Ok(Some(text.to_std_string_escaped()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unconfined `require` of a local door (issue #1611): these tests write their modules to
    /// the temp dir.
    fn trusted_files() -> Arc<ScriptFileAccess> {
        Arc::new(ScriptFileAccess::trusted_cwd())
    }
    use crate::backends::InMemoryFlowStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn test_js_engine_compiles() {
        let script = r#"
function respond(ctx) {
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(engine.is_ok());
    }

    #[test]
    fn test_js_engine_without_should_inject_still_constructs() {
        // Issue #357 Item 4: a script defining no v2 named entrypoint is legal — it's a
        // bare-expression script (Item 2). Construction only validates that the script compiles.
        let script = r#"
function some_other_function() {
    return true;
}
"#;

        let engine = JsEngine::new(script, "test-rule");
        assert!(
            engine.is_ok(),
            "bare/helper-only scripts must construct: {:?}",
            engine.err()
        );
    }

    #[test]
    fn test_js_engine_syntax_error_fails_construction() {
        let script = "function respond(ctx {  // missing paren";
        let engine = JsEngine::new(script, "test-rule");
        assert!(
            engine.is_err(),
            "a genuine syntax error must still fail construction"
        );
    }

    #[test]
    fn test_js_simple_fault_injection() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/api/test") {
        return http(503, "Service unavailable");
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = crate::util::OrderedMap::default();
        headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 503);
                assert_eq!(body, "Service unavailable");
            }
            _ => panic!("Expected error fault decision"),
        }
    }

    // Issue #327: the JS respond(ctx) Boa context caps loop iterations so a runaway script
    // terminates (Boa throws) instead of leaking its spawn_blocking thread forever. A small
    // injected limit keeps these tests fast and guarantees they can never hang.
    #[test]
    fn js_respond_loop_limit_terminates() {
        set_current_flow_store(Arc::new(InMemoryFlowStore::new(300)));
        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };
        let script = "function respond(ctx) { while (true) {} }";
        let ctx_input = ScriptCtxExtras::default().build_ctx_input(&request);
        let result = execute_js_script_inner_bounded(script, "rule", &ctx_input, 100_000);
        clear_current_flow_store();
        assert!(
            result.is_err(),
            "a runaway JS loop must hit the iteration cap and return Err, not run unbounded"
        );
    }

    #[test]
    fn js_respond_under_limit_runs() {
        set_current_flow_store(Arc::new(InMemoryFlowStore::new(300)));
        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };
        let script = "function respond(ctx) { let n = 0; for (let i = 0; i < 100; i++) { n++; } return n === 100 ? delay(1) : pass(); }";
        let ctx_input = ScriptCtxExtras::default().build_ctx_input(&request);
        let result = execute_js_script_inner_bounded(script, "rule", &ctx_input, 100_000);
        clear_current_flow_store();
        assert!(
            result.is_ok(),
            "a small-loop script under the cap must run fine: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_js_latency_fault() {
        let script = r#"
function respond(ctx) {
    return delay(1000);
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Latency { duration_ms, .. } => {
                assert_eq!(duration_ms, 1000);
            }
            _ => panic!("Expected latency fault decision"),
        }
    }

    #[test]
    fn test_js_flow_store_increment() {
        let script = r#"
function respond(ctx) {
    var attempts = ctx.state.incr("attempts");

    if (attempts <= 2) {
        return http(503, "Attempt " + attempts);
    }

    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = crate::util::OrderedMap::default();
        headers.insert("x-flow-id".to_string(), vec!["flow-123".to_string()]);

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        // First attempt should inject fault
        let result1 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result1, FaultDecision::Error { .. }));

        // Second attempt should inject fault
        let result2 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result2, FaultDecision::Error { .. }));

        // Third attempt should not inject fault
        let result3 = engine.should_inject(&request, Arc::clone(&store)).unwrap();
        assert!(matches!(result3, FaultDecision::None));
    }

    #[test]
    fn test_js_flow_store_get_set() {
        let script = r#"
function respond(ctx) {
    // Set a value
    ctx.state.set("test_key", "test_value");

    // Get it back
    var value = ctx.state.get("test_key");

    // Check if it matches
    if (value === "test_value") {
        return http(200, "Get/Set works!");
    }

    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = crate::util::OrderedMap::default();
        headers.insert("x-flow-id".to_string(), vec!["flow-123".to_string()]);

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers,
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        // Should inject fault if get/set works
        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "Get/Set works!");
            }
            _ => panic!("Expected error fault decision - get/set failed"),
        }
    }

    #[test]
    fn test_compile_js_to_bytecode() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/test") {
        return http(500, "Test error");
    }
    return pass();
}
"#;

        // Compile to bytecode
        let bytecode = compile_js_to_bytecode(script).unwrap();

        // Verify bytecode is not empty
        assert!(!bytecode.is_empty());
    }

    #[test]
    fn test_execute_js_bytecode() {
        let script = r#"
function respond(ctx) {
    if (ctx.request.path === "/api/bytecode") {
        return http(503, "Bytecode executed");
    }
    return pass();
}
"#;

        // Compile to bytecode
        let bytecode = compile_js_to_bytecode(script).unwrap();

        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut headers = crate::util::OrderedMap::default();
        headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/bytecode".to_string(),
            headers,
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result = execute_js_bytecode(&bytecode, &request, store, "bytecode-rule").unwrap();

        match result {
            FaultDecision::Error {
                status,
                body,
                rule_id,
                headers,
            } => {
                assert_eq!(status, 503);
                assert_eq!(body, "Bytecode executed");
                assert_eq!(rule_id, "bytecode-rule");
                assert!(headers.is_empty());
            }
            _ => panic!("Expected error fault decision from bytecode execution"),
        }
    }

    #[test]
    fn test_js_with_complex_body() {
        let script = r#"
function respond(ctx) {
    var body = ctx.request.json;
    if (body && body.nested && body.nested.value > 100) {
        return http(400, "Value too high: " + body.nested.value);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        // Test with high value
        let request1 = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "POST".to_string(),
            path: "/api/test".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({
                "nested": {
                    "value": 200,
                    "name": "test"
                },
                "array": [1, 2, 3]
            }),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result1 = engine.should_inject(&request1, Arc::clone(&store)).unwrap();

        match result1 {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 400);
                assert_eq!(body, "Value too high: 200");
            }
            _ => panic!("Expected error for high value"),
        }

        // Test with low value
        let request2 = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "POST".to_string(),
            path: "/api/test".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({
                "nested": {
                    "value": 50,
                    "name": "test"
                },
                "array": [4, 5, 6]
            }),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result2 = engine.should_inject(&request2, store).unwrap();
        assert!(matches!(result2, FaultDecision::None));
    }

    #[test]
    fn test_js_error_with_headers() {
        let script = r#"
function respond(ctx) {
    return http(502, "Gateway error")
        .header("X-Custom-Header", "custom-value")
        .header("X-Error-Code", "E001");
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query: Default::default(),
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error {
                status, headers, ..
            } => {
                assert_eq!(status, 502);
                assert_eq!(
                    headers.get("X-Custom-Header"),
                    Some(&"custom-value".to_string())
                );
                assert_eq!(headers.get("X-Error-Code"), Some(&"E001".to_string()));
            }
            _ => panic!("Expected error fault decision with headers"),
        }
    }

    #[test]
    fn test_js_query_params() {
        let script = r#"
function respond(ctx) {
    var name = ctx.request.query["name"];
    var page = ctx.request.query["page"];

    if (name && page) {
        return http(200, "Hello " + name + " on page " + page);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut query = crate::util::OrderedMap::default();
        query.insert("name".to_string(), vec!["Alice".to_string()]);
        query.insert("page".to_string(), vec!["42".to_string()]);

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query,
            path_params: HashMap::new(),
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "Hello Alice on page 42");
            }
            _ => panic!("Expected error fault decision with query params"),
        }
    }

    #[test]
    fn test_js_path_params() {
        let script = r#"
function respond(ctx) {
    var user_id = ctx.request.pathParams["id"];
    var action = ctx.request.pathParams["action"];

    if (user_id && action) {
        return http(200, "User " + user_id + " action: " + action);
    }
    return pass();
}
"#;

        let engine = JsEngine::new(script, "test-rule").unwrap();
        let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));

        let mut path_params = HashMap::new();
        path_params.insert("id".to_string(), "123".to_string());
        path_params.insert("action".to_string(), "update".to_string());

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "POST".to_string(),
            path: "/users/123/update".to_string(),
            headers: crate::util::OrderedMap::default(),
            body: json!({}),
            query: Default::default(),
            path_params,
        };

        let result = engine.should_inject(&request, store).unwrap();

        match result {
            FaultDecision::Error { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(body, "User 123 action: update");
            }
            _ => panic!("Expected error fault decision with path params"),
        }
    }

    // A fresh, never-reused port number per test, so the logger lines of parallel tests stay apart.
    fn test_port() -> u16 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(40_100);
        NEXT.fetch_add(1, Ordering::Relaxed) as u16
    }

    #[test]
    fn test_decorate_with_logger_arg() {
        let request = MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };
        let headers = crate::util::OrderedMap::default();

        // Script that uses logger as 3rd argument — must not throw ReferenceError
        let script = r#"function(request, response, logger) { logger.info("decorating"); response.body = "logged"; }"#;
        let result = execute_mountebank_decorate(
            script,
            &request,
            "original",
            &crate::behaviors::ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        assert!(
            result.is_ok(),
            "logger arg should not throw: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().body, "logged");
    }

    // Issue #1451: `requestFrom`, `ip` and `form` are absent keys when the caller has none, on
    // `request` and on the flattened `config` alike — never present as `undefined` or `null`.
    #[test]
    fn absent_client_fields_are_absent_keys_in_the_script_request() {
        let script = "function (config, response) { var r = config.request; response.body = \
            [('requestFrom' in r), ('ip' in r), ('form' in r), \
             ('requestFrom' in config), ('ip' in config), ('form' in config)].join('|'); }";
        let run = |request: &MountebankRequest| {
            execute_mountebank_decorate(
                script,
                request,
                "orig",
                &crate::behaviors::ResponseStatus::Code(200),
                &crate::util::OrderedMap::default(),
                &ScriptScope::detached(test_port()),
                None,
                &trusted_files(),
            )
            .expect("decorate runs")
            .body
        };

        let without = MountebankRequest {
            method: "GET".to_string(),
            path: "/p".to_string(),
            ..Default::default()
        };
        assert_eq!(run(&without), "false|false|false|false|false|false");

        let with = MountebankRequest {
            request_from: Some("127.0.0.1:50000".to_string()),
            ip: Some("127.0.0.1".to_string()),
            form: Some(crate::util::OrderedMap::from_iter([(
                "a".to_string(),
                vec!["1".to_string()],
            )])),
            ..without
        };
        assert_eq!(run(&with), "true|true|true|true|true|true");
    }

    // Issue #498: a `predicateGenerators.inject` script error must return `Err(Script)` — NOT a
    // silent empty Vec that the proxy path would turn into a match-all stub.
    #[test]
    fn predicate_generator_inject_script_error_returns_err() {
        let request = MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: "GET".to_string(),
            path: "/api/users".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let throwing = r#"function(config, logger, predicates) { throw new Error("boom"); }"#;
        let err = execute_predicate_generator_inject(throwing, &request, &[], &trusted_files())
            .expect_err("a throwing generator must return Err, not empty predicates");
        assert!(
            matches!(
                &err,
                PredicateGeneratorError::Threw { message: Some(m), .. } if m == "boom"
            ),
            "expected the thrown message, got {err:?}"
        );

        let ok_fn = r#"function(config, logger, predicates) {
            return [{ equals: { path: config.request.path } }];
        }"#;
        let preds = execute_predicate_generator_inject(ok_fn, &request, &[], &trusted_files())
            .expect("a valid generator returns Ok");
        assert_eq!(preds.len(), 1);
    }

    // Issue #498: a generator that EVALUATES fine but returns garbage (a non-array, or nothing) must
    // map to `Err(Output)` — the pre-fix code degraded both to an empty Vec, silently recording a
    // match-all stub. Same failure class as a thrown error, via a different arm.
    #[test]
    fn predicate_generator_inject_invalid_output_returns_err() {
        let request = MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: "GET".to_string(),
            path: "/api/users".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        // Returns a number → JSON.stringify gives "5", which does not parse as a predicate array.
        let non_array = r#"function(config, logger, predicates) { return 5; }"#;
        let err = execute_predicate_generator_inject(non_array, &request, &[], &trusted_files())
            .expect_err("a non-array return must fail, not silently produce empty predicates");
        assert!(
            matches!(err, PredicateGeneratorError::Output(_)),
            "expected Output error, got {err:?}"
        );

        // Returns undefined → not stringifiable at all.
        let no_return = r#"function(config, logger, predicates) { return undefined; }"#;
        let err = execute_predicate_generator_inject(no_return, &request, &[], &trusted_files())
            .expect_err(
                "a non-stringifiable return must fail, not silently produce empty predicates",
            );
        assert!(
            matches!(err, PredicateGeneratorError::Output(_)),
            "expected Output error, got {err:?}"
        );
    }

    #[test]
    fn test_decorate_with_state_arg() {
        let request = MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: "GET".to_string(),
            path: "/test".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };
        let headers = crate::util::OrderedMap::default();

        // Script that uses state as 4th argument — must not throw ReferenceError
        let script = r#"function(request, response, logger, state) { state.count = 1; response.body = "state ok"; }"#;
        let result = execute_mountebank_decorate(
            script,
            &request,
            "original",
            &crate::behaviors::ResponseStatus::Code(200),
            &headers,
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        assert!(
            result.is_ok(),
            "state arg should not throw: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().body, "state ok");
    }

    // Issue #305 gate: `config =>` decorate convention in Boa + CommonJS require().
    fn config_req() -> MountebankRequest {
        MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: "POST".to_string(),
            path: "/req".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: Some("REQ-BODY".to_string()),
            ..Default::default()
        }
    }

    // Writes a unique temp .cjs module and returns its path; caller removes it.
    fn write_temp_cjs(tag: &str, source: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rift_305_{tag}_{}_{n}.cjs", std::process::id()));
        std::fs::write(&path, source).expect("write temp module");
        path
    }

    // AC1: require() loads + runs an external module; its config.response mutation takes effect.
    #[test]
    fn test_config_decorate_require_runs_module() {
        let module = write_temp_cjs(
            "run",
            "module.exports = function (config) {\n  config.response.body = 'REQUIRE-RAN';\n  config.response.headers = config.response.headers || {};\n  config.response.headers['X-Injected-By'] = 'mod.cjs';\n};\n",
        );
        let script = format!(
            "config => {{ const s = require('{}'); s(config); }}",
            module.display()
        );
        let result = execute_mountebank_decorate(
            &script,
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        let _ = std::fs::remove_file(&module);
        let resp = result.expect("require decorate should run");
        assert_eq!(resp.body, "REQUIRE-RAN");
        assert_eq!(
            resp.headers.get("X-Injected-By").map(String::as_str),
            Some("mod.cjs")
        );
    }

    // AC2: the config convention runs in Boa without require.
    #[test]
    fn test_config_decorate_direct_field_access() {
        let script = "config => { config.response.body = 'DIRECT'; }";
        let resp = execute_mountebank_decorate(
            script,
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("direct config decorate should run");
        assert_eq!(resp.body, "DIRECT");
    }

    // AC3: config.request is exposed to the decorate.
    #[test]
    fn test_config_decorate_reads_request_body() {
        let script = "config => { config.response.body = config.request.body; }";
        let resp = execute_mountebank_decorate(
            script,
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("config.request decorate should run");
        assert_eq!(resp.body, "REQ-BODY");
    }

    // Circular require() must terminate (Node cycle-break), not recurse into a stack overflow.
    #[test]
    fn test_config_decorate_require_circular_terminates() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path_a = dir.join(format!("rift_305_cyc_a_{pid}_{n}.cjs"));
        let path_b = dir.join(format!("rift_305_cyc_b_{pid}_{n}.cjs"));
        // a <-> b require each other (the cycle); b gets a's partial exports via the cache instead
        // of recursing forever. The outer require(a) must still return a's FINAL exports.
        std::fs::write(
            &path_a,
            format!(
                "const b = require('{}');\nmodule.exports = {{ tag: 'A-LOADED' }};\n",
                path_b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &path_b,
            format!(
                "const a = require('{}');\nmodule.exports = {{ tag: 'B-LOADED' }};\n",
                path_a.display()
            ),
        )
        .unwrap();
        // The guarantee is termination: require(a) drives the a->b->a cycle, which the cache-break
        // resolves instead of recursing until the native stack overflows (which would abort the
        // process). Execution then continues and sets the body. (Exact exports of a module observed
        // mid-cycle are CommonJS-implementation-defined and out of scope here.)
        let script = format!(
            "config => {{ require('{}'); config.response.body = 'REACHED'; }}",
            path_a.display()
        );
        let result = execute_mountebank_decorate(
            &script,
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        assert_eq!(
            result
                .expect("circular require must terminate, not overflow")
                .body,
            "REACHED"
        );
    }

    // AC4: a require() of a missing module surfaces as an error, not a silent Ok/no-op.
    #[test]
    fn test_config_decorate_require_missing_errors() {
        let script = "config => { const s = require('/no/such/rift305/module.cjs'); s(config); }";
        let result = execute_mountebank_decorate(
            script,
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        assert!(
            result.is_err(),
            "a missing require() must surface as Err, not silently no-op"
        );
    }

    // =========================================================================================
    // Issue #1611: what `require()` may read, per imposter and per run.
    // =========================================================================================

    /// A scripts root holding `top.js` → `lib/a.js` → `./b.js`, `data.json`, and an `outside/`
    /// sibling with a canary module.
    struct ScriptsTree {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        outside: std::path::PathBuf,
    }

    fn scripts_tree() -> ScriptsTree {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical tempdir");
        let root = base.join("sd");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            root.join("top.js"),
            "module.exports = require('./lib/a.js');",
        )
        .unwrap();
        std::fs::write(
            root.join("lib/a.js"),
            "module.exports = 'A>' + require('./b.js') + ':' + __dirname.endsWith('lib');",
        )
        .unwrap();
        std::fs::write(root.join("lib/b.js"), "module.exports = 'B';").unwrap();
        std::fs::write(root.join("data.json"), r#"{"n": 7, "s": "json"}"#).unwrap();
        std::fs::write(outside.join("mod.js"), "module.exports = 'CANARY_1611';").unwrap();
        ScriptsTree {
            _dir: dir,
            root,
            outside,
        }
    }

    /// The body an inject reporting `require(<module>)` returns under `files`.
    fn inject_require(files: &Arc<ScriptFileAccess>, module: &str) -> String {
        let script = format!(
            "config => {{ try {{ return {{ body: 'OK:' + JSON.stringify(require({module:?})) }}; }} \
             catch (e) {{ return {{ body: 'ERR:' + String(e) }}; }} }}"
        );
        execute_mountebank_inject(
            &script,
            &mb_req("GET", "/r"),
            ScriptScope::detached(test_port()),
            None,
            files,
        )
        .expect("inject runs")
        .body
        .expect("a body")
    }

    #[test]
    fn confined_require_loads_from_the_root_and_nested_requires_from_the_module() {
        let tree = scripts_tree();
        let files = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        assert_eq!(inject_require(&files, "top.js"), "OK:\"A>B:true\"");
        assert_eq!(
            inject_require(&files, tree.root.join("lib/b.js").to_str().unwrap()),
            "OK:\"B\""
        );
        // A `.json` module is parsed, as in Node.
        assert_eq!(
            inject_require(&files, "data.json"),
            r#"OK:{"n":7,"s":"json"}"#
        );
    }

    #[test]
    fn confined_require_refuses_every_path_outside_the_root_with_one_message() {
        let tree = scripts_tree();
        let files = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let present = tree.outside.join("mod.js");
        let missing = tree.outside.join("missing.js");
        for module in [
            present.to_str().unwrap(),
            missing.to_str().unwrap(),
            "../outside/mod.js",
            "/etc/hosts",
        ] {
            let body = inject_require(&files, module);
            assert_eq!(
                body,
                format!("ERR:Error: require('{module}'): outside --scripts-dir"),
            );
        }
        assert_eq!(
            inject_require(&files, "lib/nope.js"),
            "ERR:Error: require('lib/nope.js'): no such module under --scripts-dir"
        );
    }

    #[test]
    fn refused_require_refuses_every_path() {
        let tree = scripts_tree();
        let files = Arc::new(ScriptFileAccess::Refused);
        for module in [tree.root.join("top.js"), tree.outside.join("mod.js")] {
            let module = module.to_str().unwrap().to_string();
            assert_eq!(
                inject_require(&files, &module),
                format!("ERR:Error: require('{module}'): no --scripts-dir is configured")
            );
        }
    }

    #[test]
    fn trusted_require_reads_any_path_and_nests_from_the_module() {
        let tree = scripts_tree();
        let files = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        assert_eq!(inject_require(&files, "top.js"), "OK:\"A>B:true\"");
        assert_eq!(
            inject_require(&files, tree.outside.join("mod.js").to_str().unwrap()),
            "OK:\"CANARY_1611\""
        );
    }

    // A `require` kept in a global of the shared realm answers only in the run that installed it,
    // so a later run, of this imposter or another, can never reach the earlier one's root or
    // module cache.
    #[test]
    fn a_stashed_require_is_dead_after_its_run() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let trusted = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        let stash = "config => { globalThis.__stashed = require; \
                     return { body: String(require('top.js')) }; }";
        let first = execute_mountebank_inject_in(
            &mut thread,
            stash,
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &trusted,
        )
        .expect("first run");
        assert_eq!(first.body.as_deref(), Some("A>B:true"));

        let replay = format!(
            "config => {{ try {{ return {{ body: String(__stashed({:?})) }}; }} \
             catch (e) {{ return {{ body: 'ERR:' + String(e) }}; }} }}",
            tree.outside.join("mod.js").to_str().unwrap()
        );
        // A later run of the same access shares the realm, so it can reach the stashed function,
        // which is dead.
        let second = execute_mountebank_inject_in(
            &mut thread,
            &replay,
            &mb_req("GET", "/b"),
            &ScriptScope::detached(test_port()),
            None,
            &trusted,
        )
        .expect("second run");
        let body = second.body.expect("a body");
        assert!(
            body.starts_with("ERR:") && body.contains("a script run that has ended"),
            "{body}"
        );
        assert!(!body.contains("CANARY_1611"), "{body}");

        // A run of another access does not even see the global (issue #1645).
        let refused = Arc::new(ScriptFileAccess::Refused);
        let third = execute_mountebank_inject_in(
            &mut thread,
            &replay,
            &mb_req("GET", "/c"),
            &ScriptScope::detached(test_port()),
            None,
            &refused,
        )
        .expect("third run");
        let body = third.body.expect("a body");
        assert!(
            body.starts_with("ERR:ReferenceError") && !body.contains("CANARY_1611"),
            "{body}"
        );
    }

    // Every Mountebank hook gets its own `require` (MB exposes it to all injection), each bound
    // to the access it is given, not one a previous run left on the worker.
    #[test]
    fn every_hook_gets_a_require_bound_to_its_own_access() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let confined = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let refused = Arc::new(ScriptFileAccess::Refused);

        let decorated = execute_mountebank_decorate_in(
            &mut thread,
            "config => { config.response.body = require('top.js'); }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("decorate");
        assert_eq!(decorated.body, "A>B:true");

        assert!(
            execute_predicate_inject_in(
                &mut thread,
                "config => require('top.js') === 'A>B:true'",
                &mb_req("GET", "/p"),
                &ScriptScope::detached(test_port()),
                &confined,
            )
            .expect("predicate inject")
        );
        let refused_predicate = execute_predicate_inject_in(
            &mut thread,
            "config => require('top.js') === 'A>B:true'",
            &mb_req("GET", "/p"),
            &ScriptScope::detached(test_port()),
            &refused,
        );
        assert!(
            refused_predicate.is_err_and(|e| e.to_string().contains("no --scripts-dir")),
            "the predicate's require is the refused imposter's, not the decorate's"
        );

        let generated = execute_predicate_generator_inject_in(
            &mut thread,
            "config => [{ equals: { body: require('lib/b.js') } }]",
            &mb_req("GET", "/g"),
            &[],
            &confined,
        )
        .expect("generator");
        assert_eq!(generated, vec![json!({"equals": {"body": "B"}})]);
    }

    // Issue #1645 (1): a promise job one imposter's hook leaves queued never runs in another's
    // run, where it would reach that run's `require`. Here a confined inject queues a job that
    // requires a file outside its root; a trusted imposter's `async` decorate then runs the jobs
    // on the same worker.
    #[test]
    fn a_queued_promise_job_never_runs_under_another_imposters_require() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let confined = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let trusted = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        let plant = format!(
            "config => {{ Promise.resolve().then(() => {{ \
               try {{ globalThis.__x = String(require({:?})); }} \
               catch (e) {{ globalThis.__x = 'ERR:' + String(e); }} }}); \
             return {{ body: 'planted' }}; }}",
            tree.outside.join("mod.js").to_str().unwrap()
        );
        execute_mountebank_inject_in(
            &mut thread,
            &plant,
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("plant");
        let decorated = execute_mountebank_decorate_in(
            &mut thread,
            "async config => { config.response.body = 'trusted'; }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted,
        )
        .expect("trusted async decorate");
        assert_eq!(decorated.body, "trusted");
        let read = execute_mountebank_inject_in(
            &mut thread,
            "config => ({ body: String(globalThis.__x) })",
            &mb_req("GET", "/b"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("read");
        let body = read.body.expect("a body");
        assert!(!body.contains("CANARY_1611"), "{body}");
    }

    // Issue #1645 (1), variant: a builtin one imposter patches never runs in another imposter's
    // run, so the patch cannot call that run's `require`.
    #[test]
    fn a_patched_builtin_never_runs_under_another_imposters_require() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let confined = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let trusted = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        let patch = format!(
            "config => {{ var orig = JSON.stringify; \
               JSON.stringify = function () {{ \
                 try {{ globalThis.__y = String(require({:?})); }} catch (e) {{ }} \
                 return orig.apply(this, arguments); }}; \
             return {{ body: 'patched' }}; }}",
            tree.outside.join("mod.js").to_str().unwrap()
        );
        execute_mountebank_inject_in(
            &mut thread,
            &patch,
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("patch");
        let trusted_run = execute_mountebank_inject_in(
            &mut thread,
            "config => ({ body: JSON.stringify({ a: 1 }) + ':' + typeof globalThis.__y })",
            &mb_req("GET", "/t"),
            &ScriptScope::detached(test_port()),
            None,
            &trusted,
        )
        .expect("trusted inject");
        assert_eq!(
            trusted_run.body.as_deref(),
            Some(r#"{"a":1}:undefined"#),
            "the trusted run sees neither the patch nor the other imposter's globals"
        );
        let read = execute_mountebank_inject_in(
            &mut thread,
            "config => ({ body: String(globalThis.__y) })",
            &mb_req("GET", "/b"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("read");
        let body = read.body.expect("a body");
        assert!(!body.contains("CANARY_1611"), "{body}");
    }

    // Issue #1645 (1a): `require` is a binding of the hook's own scope, as in Node, not a global
    // that code from another run could read.
    #[test]
    fn require_is_in_the_hooks_scope_and_not_a_global() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let confined = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let probe = "typeof globalThis.require + ':' + typeof require";
        let injected = execute_mountebank_inject_in(
            &mut thread,
            &format!("config => ({{ body: {probe} }})"),
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("inject");
        assert_eq!(injected.body.as_deref(), Some("undefined:function"));
        let decorated = execute_mountebank_decorate_in(
            &mut thread,
            &format!("config => {{ config.response.body = {probe}; }}"),
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("decorate");
        assert_eq!(decorated.body, "undefined:function");
        assert!(
            execute_predicate_inject_in(
                &mut thread,
                &format!("config => ({probe}) === 'undefined:function'"),
                &mb_req("GET", "/p"),
                &ScriptScope::detached(test_port()),
                &confined,
            )
            .expect("predicate inject")
        );
        let generated = execute_predicate_generator_inject_in(
            &mut thread,
            &format!("config => [{{ equals: {{ body: {probe} }} }}]"),
            &mb_req("GET", "/g"),
            &[],
            &confined,
        )
        .expect("generator");
        assert_eq!(
            generated,
            vec![json!({"equals": {"body": "undefined:function"}})]
        );
    }

    // Issue #1645 (1b): a job a hook leaves queued is discarded before the next run on the realm,
    // so no run ever executes code queued by another — even one of the same imposter.
    #[test]
    fn a_hooks_leftover_promise_jobs_are_discarded_before_the_next_run() {
        let mut thread = MbJsThread::new();
        let refused = Arc::new(ScriptFileAccess::Refused);
        execute_mountebank_inject_in(
            &mut thread,
            "config => { Promise.resolve().then(() => { globalThis.__ran = 'yes'; }); \
             return { body: 'queued' }; }",
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &refused,
        )
        .expect("queue");
        let decorated = execute_mountebank_decorate_in(
            &mut thread,
            "async config => { await null; config.response.body = String(globalThis.__ran); }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &refused,
        )
        .expect("async decorate runs its own jobs");
        assert_eq!(decorated.body, "undefined");
    }

    /// A JSON canary and an ES-module canary in a fresh directory under the working directory,
    /// where Boa's default module loader resolves from; their paths relative to it.
    fn import_canaries() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::Builder::new()
            .prefix(".rift-1645-import-")
            .tempdir_in(".")
            .expect("tempdir in cwd");
        std::fs::write(
            dir.path().join("canary.json"),
            r#"{"s":"CANARY_IMPORT_1645"}"#,
        )
        .expect("json canary");
        std::fs::write(
            dir.path().join("canary.mjs"),
            "export default 'CANARY_IMPORT_1645';",
        )
        .expect("module canary");
        let name = dir
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        (
            dir,
            format!("{name}/canary.json"),
            format!("{name}/canary.mjs"),
        )
    }

    // Review of #1645: Boa's default module loader resolves `import()` from the working
    // directory, so an `async` decorate could read any JS or JSON file under it, whatever its
    // imposter's access. No hook may load a module, on any access.
    #[test]
    fn a_hooks_dynamic_import_loads_no_module_on_any_access() {
        let (_dir, json_path, module_path) = import_canaries();
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        for files in [
            Arc::new(ScriptFileAccess::Confined(tree.root.clone())),
            Arc::new(ScriptFileAccess::Refused),
            Arc::new(ScriptFileAccess::Trusted {
                base: tree.root.clone(),
            }),
        ] {
            for import in [
                format!("import({json_path:?}, {{ with: {{ type: 'json' }} }})"),
                format!("import({module_path:?})"),
            ] {
                let decorate = format!(
                    "async config => {{ try {{ const m = await {import}; \
                       config.response.body = 'LEAK:' + JSON.stringify(m.default); }} \
                     catch (e) {{ config.response.body = 'ERR:' + String(e); }} }}"
                );
                let decorated = execute_mountebank_decorate_in(
                    &mut thread,
                    &decorate,
                    &config_req(),
                    "orig",
                    &crate::behaviors::ResponseStatus::Code(200),
                    &crate::util::OrderedMap::default(),
                    &ScriptScope::detached(test_port()),
                    None,
                    &files,
                )
                .expect("decorate runs");
                assert!(
                    decorated.body.starts_with("ERR:") && !decorated.body.contains("CANARY"),
                    "{files:?} {import}: {}",
                    decorated.body
                );
            }
        }
    }

    // The same for every other context the engine builds: `_rift.script` and the parse contexts
    // share the bounded one.
    #[test]
    fn the_bounded_script_context_loads_no_module() {
        let (_dir, json_path, _module_path) = import_canaries();
        let mut context = bounded_js_context();
        let script = format!(
            "var __out = 'pending'; \
             import({json_path:?}, {{ with: {{ type: 'json' }} }}) \
               .then(m => {{ __out = 'LEAK:' + JSON.stringify(m.default); }}, \
                     e => {{ __out = 'ERR:' + String(e); }});"
        );
        context
            .eval(Source::from_bytes(script.as_bytes()))
            .expect("script runs");
        context.run_jobs().expect("jobs run");
        let out = context
            .global_object()
            .get(js_string!("__out"), &mut context)
            .expect("__out")
            .display()
            .to_string();
        assert!(out.contains("ERR:") && !out.contains("CANARY"), "{out}");
    }

    fn mb_req(method: &str, path: &str) -> MountebankRequest {
        MountebankRequest {
            mode: Some(ResponseMode::Text),
            method: method.to_string(),
            path: path.to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        }
    }

    // Issue #1369: a numeric-string statusCode reads like every other `statusCode` (the handler
    // reads the string, #1635); an absent one is `None`, left to the imposter's `defaultResponse`.
    #[test]
    fn inject_status_code_accepts_a_numeric_string() {
        let script = r#"function(config) { return { statusCode: "201", body: "b" }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/s"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run");
        assert_eq!(
            resp.status_code,
            Some(crate::behaviors::ResponseStatus::Text("201".to_string()))
        );
        let script = r#"function(config) { return { body: "b" }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/s"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run");
        assert_eq!(resp.status_code, None);
    }

    /// The `statusCode` an inject returns, read with no request context.
    fn inject_status(expr: &str) -> Option<crate::behaviors::ResponseStatus> {
        let script = format!("function(config) {{ return {{ statusCode: {expr}, body: 'b' }}; }}");
        execute_mountebank_inject(
            &script,
            &mb_req("GET", "/1635"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run")
        .status_code
    }

    // Issue #1635: a string Node cannot read is kept, for the handler to refuse with a 500, not
    // dropped to the `defaultResponse` status.
    #[test]
    fn inject_status_code_keeps_a_non_numeric_string() {
        assert_eq!(
            inject_status("'abc'"),
            Some(crate::behaviors::ResponseStatus::Text("abc".to_string()))
        );
    }

    // Issue #1635: a number is a code only while it is an integer within `u16`; any other value
    // that is not falsy is its JavaScript string, which Node refuses or reads, never saturated.
    #[test]
    fn inject_status_code_reads_numbers_as_node_does() {
        use crate::behaviors::ResponseStatus::{Code, Text};
        assert_eq!(inject_status("404"), Some(Code(404)));
        assert_eq!(inject_status("0"), Some(Code(0)));
        assert_eq!(inject_status("-1"), Some(Text("-1".to_string())));
        assert_eq!(inject_status("70000"), Some(Text("70000".to_string())));
        assert_eq!(inject_status("404.7"), Some(Text("404.7".to_string())));
        assert_eq!(inject_status("true"), Some(Text("true".to_string())));
        assert_eq!(
            inject_status("{}"),
            Some(Text("[object Object]".to_string()))
        );
        assert_eq!(inject_status("NaN"), None);
        assert_eq!(inject_status("false"), None);
        assert_eq!(inject_status("null"), None);
        assert_eq!(inject_status("undefined"), None);
    }

    // Issue #1635: a number or boolean header value is sent as JavaScript prints it, as Node's
    // `writeHead` sends it, instead of being dropped.
    #[test]
    fn inject_numeric_header_values_are_sent() {
        let script = r#"function(config) {
            return { headers: { "X-Count": 5, "X-Flag": true, "X-Name": "n" }, body: "b" };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/1635"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run");
        let headers = resp.headers.expect("headers returned");
        assert_eq!(headers.get("X-Count").map(String::as_str), Some("5"));
        assert_eq!(headers.get("X-Flag").map(String::as_str), Some("true"));
        assert_eq!(headers.get("X-Name").map(String::as_str), Some("n"));
    }

    // Mountebank serves an object body with `JSON.stringify`, which throws for a cycle or a BigInt,
    // and answers 500 (`baseHttpServer.js:36`, `:150-155`). The inject fails here too, instead of
    // overflowing the stack or serving `null`.
    #[test]
    fn inject_unserializable_body_is_an_error() {
        for body in [
            "(() => { const o = {}; o.self = o; return o; })()",
            "{ n: 10n }",
        ] {
            let script = format!("function(config) {{ return {{ body: {body} }}; }}");
            let result = execute_mountebank_inject(
                &script,
                &mb_req("GET", "/1635"),
                ScriptScope::detached(test_port()),
                None,
                &trusted_files(),
            );
            assert!(result.is_err(), "{body}: {:?}", result.map(|r| r.body));
        }
    }

    // What `JSON.stringify` drops (a function, `undefined`) is dropped from the body too, and a
    // `toJSON` is honoured.
    #[test]
    fn inject_object_body_is_serialized_as_json_stringify_does() {
        let script = r#"function(config) {
            return { body: { b: 1, f: function () {}, u: undefined, d: { toJSON() { return 'D'; } } } };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/1635"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run");
        assert_eq!(resp.body_text(), r#"{"b":1,"d":"D"}"#);
    }

    // Node's well-formed `JSON.stringify` escapes a lone surrogate as `\udXXX`, and Mountebank
    // serves that text; so does Rift.
    #[test]
    fn inject_body_with_a_lone_surrogate_is_served_escaped() {
        let script = r#"function(config) { return { body: { s: '\ud800', t: 'ok' } }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/1635"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject should run");
        assert_eq!(resp.body_text(), r#"{"s":"\ud800","t":"ok"}"#);
    }

    /// An inject whose body nests `depth` objects, built without recursion.
    fn nested_body_inject(depth: usize) -> Result<MountebankInjectResponse> {
        let script = format!(
            "function(config) {{ let o = {{}}; for (let i = 1; i < {depth}; i++) {{ o = {{ a: o }}; }} return {{ body: o }}; }}"
        );
        execute_mountebank_inject(
            &script,
            &mb_req("GET", "/1635"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
    }

    // A body nested past the cap is refused, rather than recursing until the stack overflows;
    // one at the cap is served.
    #[test]
    fn inject_body_nesting_is_capped() {
        let at_cap = nested_body_inject(MAX_JSON_DEPTH).expect("at the cap");
        assert!(at_cap.body_text().starts_with(r#"{"a":{"a":"#));
        assert!(nested_body_inject(MAX_JSON_DEPTH + 1).is_err());
        assert!(nested_body_inject(200_000).is_err());
    }

    #[test]
    fn js_to_json_refuses_nesting_past_the_cap() {
        let mut context = Context::default();
        let deep = |depth: usize, context: &mut Context| {
            context
                .eval(Source::from_bytes(&format!(
                    "(() => {{ let o = {{}}; for (let i = 1; i < {depth}; i++) {{ o = [o]; }} return o; }})()"
                )))
                .expect("evaluates")
        };
        let at_cap = deep(MAX_JSON_DEPTH, &mut context);
        assert!(js_to_json(&mut context, &at_cap).is_ok());
        let past = deep(200_000, &mut context);
        assert!(js_to_json(&mut context, &past).is_err());
    }

    #[test]
    fn js_to_json_refuses_a_cycle() {
        let mut context = Context::default();
        let cyclic = context
            .eval(Source::from_bytes("const o = { a: [1] }; o.a.push(o); o"))
            .expect("evaluates");
        assert!(js_to_json(&mut context, &cyclic).is_err());
        // A shared, acyclic reference is not a cycle.
        let shared = context
            .eval(Source::from_bytes("const s = {}; ({ x: s, y: [s, s] })"))
            .expect("evaluates");
        assert_eq!(
            js_to_json(&mut context, &shared).expect("acyclic"),
            serde_json::json!({ "x": {}, "y": [{}, {}] })
        );
    }

    // (a) v2 config convention: function(config) { ...config.request.path... }
    #[test]
    fn inject_v2_config_request_path() {
        let script =
            r#"function(config) { return { statusCode: 200, body: config.request.path }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/a-path"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("v2 config convention should run");
        assert_eq!(resp.body_text(), "/a-path");
    }

    // Issue #476 AC5: the MB wrapper script is parsed at most once per pool worker per source —
    // reruns of the SAME source hit the parsed-Script cache. The pool has at most 16 workers, so
    // 64 runs parse at most 16 times with the cache and exactly 64 times without it. The body
    // marker is unique per run (port-derived), so concurrent tests can't touch this count.
    #[test]
    fn mb_script_parse_cached_across_calls() {
        let port = test_port();
        let scope = ScriptScope::detached(port);
        let marker = format!("parse-cache-{port}");
        let script =
            format!("function(config) {{ return {{ statusCode: 200, body: '{marker}' }}; }}");
        for _ in 0..64 {
            let resp = execute_mountebank_inject(
                &script,
                &mb_req("GET", "/c"),
                scope.clone(),
                None,
                &trusted_files(),
            )
            .expect("inject run");
            assert_eq!(resp.body_text(), marker);
        }
        let parses = mb_script_parse_count_containing(&marker);
        assert!(
            (1..=16).contains(&parses),
            "64 same-source runs must parse at most once per pool worker (≤16), got {parses}"
        );
    }

    // Issue #1365: a request without a body reaches every script as `""`, as in Mountebank, not
    // `undefined`.
    #[test]
    fn inject_sees_empty_string_body_for_bodyless_request() {
        let script = r#"function(config) { return { statusCode: 200, body: typeof config.request.body + ':' + JSON.stringify(config.request.body) }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/nobody"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(resp.body_text(), r#"string:"""#);
    }

    #[test]
    fn decorate_sees_empty_string_request_body() {
        // Every decorate form builds the request through `create_mountebank_request_object`.
        let script = r#"function(request, response) { response.body = typeof request.body + ':' + JSON.stringify(request.body); }"#;
        let result = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/nobody"),
            "original",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("decorate runs");
        assert_eq!(result.body, r#"string:"""#);
    }

    /// Issue #1584: a body that did not decode is Mountebank's `undefined` body, so the request
    /// object has no `body` property.
    #[test]
    fn inject_sees_no_body_when_the_body_did_not_decode() {
        let script = r#"function(config) { return { body: typeof config.request.body + ':' + ('body' in config.request) }; }"#;
        let request = MountebankRequest {
            body_undefined: true,
            ..mb_req("POST", "/gz")
        };
        let resp = execute_mountebank_inject(
            script,
            &request,
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(resp.body_text(), "undefined:false");
    }

    #[test]
    fn predicate_inject_sees_empty_string_body() {
        let matched = execute_predicate_inject(
            r#"function(config) { return config.request.body === ""; }"#,
            &mb_req("GET", "/nobody"),
            ScriptScope::detached(test_port()),
            &trusted_files(),
        )
        .expect("predicate inject runs");
        assert!(matched);
    }

    /// Issue #1368: a request whose headers the listener Title-Cased, as it does for every imposter.
    fn mb_req_with_headers() -> MountebankRequest {
        let mut request = mb_req("GET", "/h");
        request.headers.insert(
            "Content-Type".to_string(),
            vec!["application/json".to_string()],
        );
        request
            .headers
            .insert("X-Id".to_string(), vec!["7".to_string()]);
        request
    }

    #[test]
    fn inject_headers_lookup_is_case_insensitive() {
        let script = r#"function(config) { var h = config.request.headers; return { body: [h['content-type'], h['CONTENT-TYPE'], h['Content-Type'], String(h['x-missing'])].join('|') }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(
            resp.body_text(),
            "application/json|application/json|application/json|undefined"
        );
    }

    #[test]
    fn inject_headers_in_and_has_own_property_ignore_case() {
        let script = r#"function(config) { var h = config.request.headers; return { body: ('x-id' in h) + ',' + h.hasOwnProperty('x-id') + ',' + Object.prototype.hasOwnProperty.call(h, 'X-ID') + ',' + ('nope' in h) }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(resp.body_text(), "true,true,true,false");
    }

    #[test]
    fn inject_headers_enumerate_title_case() {
        // Enumeration is unchanged: the names the listener stored.
        let script = r#"function(config) { var h = config.request.headers; return { body: JSON.stringify(Object.keys(h).sort()) + ' ' + JSON.stringify(h) .length }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert!(
            resp.body_text().starts_with(r#"["Content-Type","X-Id"] "#),
            "{}",
            resp.body_text()
        );
    }

    /// A write or delete under another spelling acts on the stored header: no hidden duplicate.
    #[test]
    fn inject_headers_write_and_delete_ignore_case() {
        let script = r#"function(config) {
            var h = config.request.headers;
            h['content-type'] = 'text/plain';
            h['content-type'] = 'text/csv';
            delete h['x-id'];
            h['x-new'] = 'n';
            return { body: [h['Content-Type'], String(h['X-Id']), ('x-id' in h), JSON.stringify(Object.keys(h).sort())].join('|') };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(
            resp.body_text(),
            r#"text/csv|undefined|false|["Content-Type","x-new"]"#
        );
    }

    /// A frozen headers object still answers lookups; a descriptor for another spelling would
    /// break a Proxy invariant on a non-extensible target, so that case reports exact names only.
    #[test]
    fn inject_headers_frozen_does_not_throw() {
        let script = r#"function(config) {
            var h = Object.freeze(config.request.headers);
            return { body: [h['x-id'], h.hasOwnProperty('X-Id'), h.hasOwnProperty('x-id')].join('|') };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("inject runs");
        assert_eq!(resp.body_text(), "7|true|false");
    }

    #[test]
    fn predicate_inject_and_decorate_share_case_insensitive_headers() {
        let matched = execute_predicate_inject(
            r#"function(config) { return config.request.headers['x-id'] === '7'; }"#,
            &mb_req_with_headers(),
            ScriptScope::detached(test_port()),
            &trusted_files(),
        )
        .expect("predicate inject runs");
        assert!(matched);
        let decorated = execute_mountebank_decorate(
            r#"function(request, response) { response.body = request.headers['content-type']; }"#,
            &mb_req_with_headers(),
            "original",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("decorate runs");
        assert_eq!(decorated.body, "application/json");
    }

    // (b) legacy positional: function(request, state, logger) { ...request.path... }
    #[test]
    fn inject_legacy_positional_request_path() {
        let script = r#"function(request, state, logger) { return { statusCode: 200, body: request.path }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/legacy"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("legacy positional convention should run");
        assert_eq!(resp.body_text(), "/legacy");
    }

    // (c) flattened: function(config) { ...config.path... } (no config.request access)
    #[test]
    fn inject_flattened_config_path() {
        let script = r#"function(config) { return { statusCode: 200, body: config.path + " " + config.method }; }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("POST", "/flat"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("flattened config fields should be readable");
        assert_eq!(resp.body_text(), "/flat POST");
    }

    // (d) the `var req = config.request || config;` shim used by scripts written to run under
    // either the old or new convention.
    #[test]
    fn inject_request_or_config_shim() {
        let script = r#"function(config) {
            var req = config.request || config;
            return { statusCode: 200, body: req.path };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/shim"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("request-or-config shim should run");
        assert_eq!(resp.body_text(), "/shim");
    }

    // (e) hybrid: function(config, state) { ... } — a script naming the 2nd positional param
    // "state" (matching Mountebank's legacy name) still works positionally.
    #[test]
    fn inject_hybrid_config_state() {
        let script = r#"function(config, state) {
            state.hits = (state.hits || 0) + 1;
            return { statusCode: 200, body: "hits:" + state.hits };
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/hybrid"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("hybrid config+state signature should run");
        assert_eq!(resp.body_text(), "hits:1");
    }

    // The async-callback convention: `done` (arg 4) is called instead of returning a value;
    // `config.callback` is an alias of the same function, so either spelling works.
    #[test]
    fn inject_callback_style_done() {
        let script = r#"function(config, injectState, logger, done) {
            done({ statusCode: 200, body: "via-done" });
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/cb"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("callback-style inject should run");
        assert_eq!(resp.body_text(), "via-done");
    }

    #[test]
    fn inject_callback_style_config_callback_alias() {
        let script = r#"function(config) {
            config.callback({ statusCode: 200, body: "via-config-callback" });
        }"#;
        let resp = execute_mountebank_inject(
            script,
            &mb_req("GET", "/cb2"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("config.callback alias should run");
        assert_eq!(resp.body_text(), "via-config-callback");
    }

    // Predicate inject: same five conventions, adapted to a boolean-returning function.
    #[test]
    fn predicate_v2_config_request_path() {
        let script = r#"function(config) { return config.request.path === "/match"; }"#;
        assert!(
            execute_predicate_inject(
                script,
                &mb_req("GET", "/match"),
                ScriptScope::detached(test_port()),
                &trusted_files()
            )
            .unwrap()
        );
    }

    #[test]
    fn predicate_legacy_positional() {
        let script = r#"function(request, logger, state) { return request.path === "/match"; }"#;
        assert!(
            execute_predicate_inject(
                script,
                &mb_req("GET", "/match"),
                ScriptScope::detached(test_port()),
                &trusted_files()
            )
            .unwrap()
        );
    }

    #[test]
    fn predicate_flattened_config_path() {
        let script = r#"function(config) { return config.path === "/match"; }"#;
        assert!(
            execute_predicate_inject(
                script,
                &mb_req("GET", "/match"),
                ScriptScope::detached(test_port()),
                &trusted_files()
            )
            .unwrap()
        );
    }

    #[test]
    fn predicate_request_or_config_shim() {
        let script = r#"function(config) {
            var req = config.request || config;
            return req.path === "/match";
        }"#;
        assert!(
            execute_predicate_inject(
                script,
                &mb_req("GET", "/match"),
                ScriptScope::detached(test_port()),
                &trusted_files()
            )
            .unwrap()
        );
    }

    #[test]
    fn predicate_hybrid_config_state() {
        let script = r#"function(config, state) {
            state.checked = true;
            return config.method === "GET";
        }"#;
        assert!(
            execute_predicate_inject(
                script,
                &mb_req("GET", "/match"),
                ScriptScope::detached(test_port()),
                &trusted_files()
            )
            .unwrap()
        );
    }

    // Decorate: same five conventions.
    #[test]
    fn decorate_v2_config_request_path() {
        let script = r#"function(config, response) { response.body = config.request.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-a"),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("v2 config decorate should run");
        assert_eq!(resp.body, "/dec-a");
    }

    #[test]
    fn decorate_legacy_positional() {
        let script = r#"function(request, response, logger) { response.body = request.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-legacy"),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("legacy positional decorate should run");
        assert_eq!(resp.body, "/dec-legacy");
    }

    #[test]
    fn decorate_flattened_config_path() {
        let script = r#"function(config, response) { response.body = config.path; }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-flat"),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("flattened config decorate should run");
        assert_eq!(resp.body, "/dec-flat");
    }

    #[test]
    fn decorate_request_or_config_shim() {
        let script = r#"function(config, response) {
            var req = config.request || config;
            response.body = req.path;
        }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-shim"),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("request-or-config shim decorate should run");
        assert_eq!(resp.body, "/dec-shim");
    }

    #[test]
    fn decorate_hybrid_config_state() {
        let script = r#"function(config, response, logger, state) {
            state.decorated = true;
            response.body = "decorated:" + config.method;
        }"#;
        let resp = execute_mountebank_decorate(
            script,
            &mb_req("GET", "/dec-hybrid"),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("hybrid config+state decorate should run");
        assert_eq!(resp.body, "decorated:GET");
    }

    // State written by a predicate inject must be visible to a later response inject on the same
    // imposter port (issue #355 Item 0: config.state / imposterState is shared per-port).
    #[test]
    fn state_shared_between_predicate_and_response_inject() {
        let port = test_port();
        let scope = ScriptScope::detached(port);
        let predicate_script =
            r#"function(config) { config.state.seen = "from-predicate"; return true; }"#;
        assert!(
            execute_predicate_inject(
                predicate_script,
                &mb_req("GET", "/shared"),
                scope.clone(),
                &trusted_files()
            )
            .unwrap()
        );

        let inject_script = r#"function(config) { return { statusCode: 200, body: config.state.seen || "missing" }; }"#;
        let resp = execute_mountebank_inject(
            inject_script,
            &mb_req("GET", "/shared"),
            scope.clone(),
            None,
            &trusted_files(),
        )
        .expect("response inject should run");
        assert_eq!(
            resp.body_text(),
            "from-predicate",
            "state written by a predicate inject must be visible to a later response inject \
             on the same imposter port"
        );
    }

    // Complement of the sharing contract: two DISTINCT imposters (distinct state keys) must NOT
    // share Mountebank inject state (issue #439). State written under one imposter's key is
    // invisible to a response inject running under another's — auto-bind imposters, which the
    // manager gives distinct bound ports, therefore never clobber each other's script state.
    #[test]
    fn mountebank_inject_state_isolated_between_distinct_imposters() {
        let port_a = test_port();
        let scope_a = ScriptScope::detached(port_a);
        let port_b = test_port();
        let scope_b = ScriptScope::detached(port_b);
        assert_ne!(
            port_a, port_b,
            "distinct imposters must have distinct state keys"
        );

        // Imposter A writes state via a predicate inject.
        let writer = r#"function(config) { config.state.marker = "from-A"; return true; }"#;
        assert!(
            execute_predicate_inject(
                writer,
                &mb_req("GET", "/iso"),
                scope_a.clone(),
                &trusted_files()
            )
            .unwrap()
        );

        // Imposter B reads state via a response inject: it must see its own (empty) state, not A's.
        let reader = r#"function(config) { return { statusCode: 200, body: config.state.marker || "isolated" }; }"#;
        let resp_b = execute_mountebank_inject(
            reader,
            &mb_req("GET", "/iso"),
            scope_b.clone(),
            None,
            &trusted_files(),
        )
        .expect("response inject should run");
        assert_eq!(
            resp_b.body_text(),
            "isolated",
            "state written under imposter A's key must be invisible to imposter B (issue #439)"
        );

        // Sanity: A still sees its own state — isolation didn't wipe it.
        let resp_a = execute_mountebank_inject(
            reader,
            &mb_req("GET", "/iso"),
            scope_a.clone(),
            None,
            &trusted_files(),
        )
        .expect("response inject should run");
        assert_eq!(resp_a.body_text(), "from-A");
        // (Decorate reads the same scope's cell, so its isolation follows from the same mechanism
        // proven here.)
    }

    // Issue #477: concurrent injects on the SAME imposter must not lose state updates. With the old
    // read-clone-mutate-overwrite (lock released across script execution) N racing increments lose
    // updates; running every one of the imposter's runs on one worker over one live state object
    // (issue #1678) makes the final count exactly N.
    #[test]
    fn concurrent_injects_do_not_lose_state_updates() {
        let port = test_port();
        let scope = ScriptScope::detached(port);
        let incr = r#"function(config) {
            config.state.count = (config.state.count || 0) + 1;
            return { statusCode: 200, body: String(config.state.count) };
        }"#;
        // A predicate inject that increments the same shared counter — so the test races TWO exec
        // fns (response inject + predicate inject) contending on the same port's cell.
        let pred_incr = r#"function(config) {
            config.state.count = (config.state.count || 0) + 1;
            return true;
        }"#;
        let n: usize = 40;
        std::thread::scope(|s| {
            for i in 0..n {
                let scope = scope.clone();
                s.spawn(move || {
                    if i % 2 == 0 {
                        execute_mountebank_inject(
                            incr,
                            &mb_req("POST", "/count"),
                            scope.clone(),
                            None,
                            &trusted_files(),
                        )
                        .expect("inject should run");
                    } else {
                        execute_predicate_inject(
                            pred_incr,
                            &mb_req("POST", "/count"),
                            scope.clone(),
                            &trusted_files(),
                        )
                        .expect("predicate inject should run");
                    }
                });
            }
        });

        let read = r#"function(config) {
            return { statusCode: 200, body: String(config.state.count || 0) };
        }"#;
        let final_resp = execute_mountebank_inject(
            read,
            &mb_req("GET", "/count"),
            scope.clone(),
            None,
            &trusted_files(),
        )
        .expect("read inject should run");
        assert_eq!(
            final_resp.body_text(),
            n.to_string(),
            "every concurrent increment must persist (no lost updates)"
        );
    }

    // Issue #743 (RFC-712 pre-work), reworked by #1726: an imposter's script state is one cell that
    // every pool worker reads and writes, never a per-worker copy, and an imposter re-created on the
    // same port gets a cell of its own. State accumulated from many workers stays with the first
    // scope; the new one starts fresh on every worker.
    #[test]
    fn state_is_one_cell_for_every_pool_worker_and_a_new_scope_starts_fresh() {
        let port = test_port();
        let scope = ScriptScope::detached(port);
        let incr = r#"function(config) {
            config.state.count = (config.state.count || 0) + 1;
            return true;
        }"#;
        let n: usize = 32;
        std::thread::scope(|s| {
            for _ in 0..n {
                let scope = scope.clone();
                s.spawn(move || {
                    execute_predicate_inject(
                        incr,
                        &mb_req("POST", "/gen"),
                        scope.clone(),
                        &trusted_files(),
                    )
                    .expect("predicate inject should run");
                });
            }
        });
        let read = r#"function(config) {
            return { statusCode: 200, body: String(config.state.count || 0) };
        }"#;
        let before = execute_mountebank_inject(
            read,
            &mb_req("GET", "/gen"),
            scope.clone(),
            None,
            &trusted_files(),
        )
        .expect("read inject should run");
        assert_eq!(
            before.body_text(),
            n.to_string(),
            "precondition: increments from every worker persisted into the shared cell"
        );

        // The imposter re-created on the same port: its own cell.
        let recreated = ScriptScope::detached(port);

        // A recreated imposter on the same port must see fresh state from EVERY worker. Reads
        // run concurrently so multiple pool workers serve them — a worker-cached copy of the
        // old state would resurface here.
        std::thread::scope(|s| {
            for _ in 0..8 {
                let scope = recreated.clone();
                s.spawn(move || {
                    let resp = execute_mountebank_inject(
                        read,
                        &mb_req("GET", "/gen"),
                        scope.clone(),
                        None,
                        &trusted_files(),
                    )
                    .expect("read inject should run");
                    assert_eq!(
                        resp.body_text(),
                        "0",
                        "the re-created imposter starts with fresh state on every worker"
                    );
                });
            }
        });
        let kept =
            execute_mountebank_inject(read, &mb_req("GET", "/gen"), scope, None, &trusted_files())
                .expect("read inject should run");
        assert_eq!(
            kept.body_text(),
            n.to_string(),
            "the first imposter's state is its own"
        );
    }

    // =========================================================================================
    // Issue #355 Item 1: native logger routes to `tracing`.
    // =========================================================================================

    /// Minimal capturing `tracing::Subscriber` (no external test-capture crate needed): records
    /// every event's target + fields as one formatted string per event.
    struct CapturingSubscriber {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={:?} ", field.name(), value));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push_str(&format!("{}={} ", field.name(), value));
                }
            }
            let mut visitor = Visitor(format!("target={} ", event.metadata().target()));
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // The native logger's methods are callable without throwing, AND the messages actually reach
    // the process's tracing subscriber at target "rift::script", tagged with the imposter port and
    // — when the caller provides one — the stub id (issue #355 Item 1 + AC1).
    #[test]
    fn mb_logger_routes_to_tracing() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: Arc::clone(&events),
        };
        let port = test_port();
        let scope = ScriptScope::detached(port);

        let script = r#"function(config) {
            config.logger.debug("dbg-msg");
            config.logger.info("hello-from-script");
            config.logger.warn("warn-msg");
            config.logger.error("err-msg");
            return { statusCode: 200, body: "logged" };
        }"#;

        let result = tracing::subscriber::with_default(subscriber, || {
            execute_mountebank_inject(
                script,
                &mb_req("GET", "/log"),
                scope.clone(),
                Some("stub-log-42"),
                &trusted_files(),
            )
        });

        assert!(
            result.is_ok(),
            "logger calls must not throw: {:?}",
            result.err()
        );

        let logs = events.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.contains("rift::script") && l.contains("hello-from-script")),
            "expected an info event carrying the script's message, got: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l.contains(&port.to_string())),
            "expected the imposter port to be tagged on a logged event, got: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l.contains("stub-log-42")),
            "expected the stub id to be tagged on a logged event when provided, got: {logs:?}"
        );
    }

    // =========================================================================================
    // Issue #355 Item 3: Boa RuntimeLimits on every MB JS hook Context.
    // =========================================================================================

    // An inject fn with a genuine infinite loop must terminate (Err), not hang, because
    // `bounded_js_context()` caps loop iterations on every Context these hooks build.
    #[test]
    fn mb_inject_infinite_loop_terminates() {
        let script = r#"function(config) { while (true) {} }"#;
        let result = execute_mountebank_inject(
            script,
            &mb_req("GET", "/loop"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        assert!(
            result.is_err(),
            "an infinite loop in an inject fn must terminate with an error, not hang"
        );
    }

    // =========================================================================================
    // Issue #355 Item 5 (AC5): a throwing inject fn surfaces as an Err (the handler maps this to
    // a Mountebank-shaped 500, issue #1581 — see issue_355_inject_error_parity.rs for the end-to-end assertion).
    // =========================================================================================
    #[test]
    fn mb_inject_throwing_returns_err() {
        let script = r#"function(config) { throw new Error('boom-inject'); }"#;
        let result = execute_mountebank_inject(
            script,
            &mb_req("GET", "/throw"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        );
        let err = result.expect_err("a throwing inject must surface as Err, not silently succeed");
        assert!(
            err.to_string().contains("boom-inject"),
            "the error must carry the script's failure message, got: {err}"
        );
    }

    // Issue #1581: Mountebank's `data` is the thrown value's `message`; absent when it has none.
    fn thrown_data(body: &str) -> Option<String> {
        let script = format!("function(config) {{ {body} }}");
        let err = execute_mountebank_inject(
            &script,
            &mb_req("GET", "/throw"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect_err("a throwing inject must be Err");
        err.downcast_ref::<InjectionFailure>()
            .expect("an InjectionFailure")
            .data
            .clone()
    }

    #[test]
    fn inject_failure_data_is_the_thrown_message() {
        assert_eq!(thrown_data("throw new Error('x');"), Some("x".into()));
        assert_eq!(thrown_data("throw new TypeError('t');"), Some("t".into()));
        assert_eq!(
            thrown_data("return nosuch.x;"),
            Some("nosuch is not defined".into())
        );
        assert_eq!(thrown_data("throw {message: 7};"), Some("7".into()));
    }

    #[test]
    fn inject_failure_data_is_absent_without_a_message() {
        assert_eq!(thrown_data("throw 'str';"), None);
        assert_eq!(thrown_data("throw 42;"), None);
        assert_eq!(thrown_data("throw {};"), None);
    }

    #[test]
    fn predicate_inject_failure_carries_the_thrown_message() {
        let err = execute_predicate_inject(
            "function(config) { throw new Error('pboom'); }",
            &mb_req("GET", "/p"),
            ScriptScope::detached(test_port()),
            &trusted_files(),
        )
        .expect_err("a throwing predicate must be Err");
        let failure = err
            .downcast_ref::<PredicateInjectionError>()
            .expect("a PredicateInjectionError");
        assert_eq!(failure.data(), Some("pboom"));
    }

    // ============================================
    // Issue #357: unified ctx, v2 respond entrypoint, result constructors (JS)
    // ============================================
    mod v2 {
        use super::*;

        fn req(headers: HashMap<String, String>, raw_body: Option<&str>) -> ScriptRequest {
            ScriptRequest {
                mode: ResponseMode::Text,
                method: "POST".to_string(),
                path: "/api/orders".to_string(),
                headers: headers.into_iter().map(|(k, v)| (k, vec![v])).collect(),
                body: json!(null),
                query: Default::default(),
                path_params: HashMap::new(),
                raw_body: raw_body.map(|s| s.to_string()),
            }
        }

        fn store() -> Arc<dyn FlowStore> {
            Arc::new(InMemoryFlowStore::new(300))
        }

        fn run_respond(script: &str, request: &ScriptRequest) -> Result<FaultDecision> {
            let engine = JsEngine::new(script, "v2-rule").unwrap();
            engine.should_inject(request, store())
        }

        /// Run a `respond` script returning `http(200, ...)` and yield the JSON body string.
        fn run_body(script: &str) -> String {
            match run_respond(script, &req(HashMap::new(), None)).unwrap() {
                FaultDecision::Error { body, .. } => body,
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        // --- ctx.state.ttl(key, seconds) + clear() (issue #530) ---

        // Regression for the original #530 report: `ttl("k", 1)` used to throw
        // `TypeError: seconds must be a number`. It now dispatches to the per-key form.
        #[test]
        fn per_key_ttl_no_longer_type_errors_and_returns_true() {
            let body = run_body(
                r#"function respond(ctx) {
                    ctx.state.set("k", "v");
                    return http(200, { ok: ctx.state.ttl("k", 100) });
                }"#,
            );
            assert!(
                body.contains("true"),
                "per-key ttl on existing key -> true, got {body}"
            );
        }

        #[test]
        fn per_key_ttl_absent_key_returns_false() {
            let body = run_body(
                r#"function respond(ctx) { return http(200, { ok: ctx.state.ttl("absent", 60) }); }"#,
            );
            assert!(
                body.contains("false"),
                "per-key ttl on absent key -> false, got {body}"
            );
        }

        #[test]
        fn per_key_ttl_zero_deletes_the_key() {
            let body = run_body(
                r#"function respond(ctx) {
                    ctx.state.set("k", "v");
                    ctx.state.ttl("k", 0);
                    return http(200, { present: ctx.state.exists("k") });
                }"#,
            );
            assert!(
                body.contains("false"),
                "ttl(key, 0) deletes the key, got {body}"
            );
        }

        #[test]
        fn flow_level_ttl_still_dispatches() {
            let body = run_body(
                r#"function respond(ctx) { return http(200, { ok: ctx.state.ttl(60) }); }"#,
            );
            assert!(
                body.contains("true"),
                "flow-level ttl(seconds) still works, got {body}"
            );
        }

        #[test]
        fn clear_empties_the_flow() {
            let body = run_body(
                r#"function respond(ctx) {
                    ctx.state.set("a", 1);
                    ctx.state.set("b", 2);
                    ctx.state.clear();
                    return http(200, { a: ctx.state.exists("a"), b: ctx.state.exists("b") });
                }"#,
            );
            assert!(
                !body.contains("true"),
                "clear() removes every key, got {body}"
            );
        }

        // The bad-arg error names BOTH signatures so the #530 confusion can't recur.
        #[test]
        fn ttl_bad_arg_error_names_both_signatures() {
            let err = run_respond(
                r#"function respond(ctx) { return http(200, { v: ctx.state.ttl(true) }); }"#,
                &req(HashMap::new(), None),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("ttl(seconds)") && err.contains("ttl(key, seconds)"),
                "error must name both signatures, got: {err}"
            );
        }

        #[test]
        fn respond_named_wrapper() {
            let script = r#"
                function respond(ctx) {
                    return http(503, { error: "unavailable" });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error {
                    status,
                    body,
                    headers,
                    ..
                } => {
                    assert_eq!(status, 503);
                    assert!(body.contains("unavailable"));
                    assert_eq!(
                        headers.get("Content-Type").map(String::as_str),
                        Some("application/json")
                    );
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        #[test]
        fn respond_bare_expression() {
            let script = r#"
                (ctx.request.method === "POST") ? http(503, { error: "unavailable" }) : pass()
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        #[test]
        fn ctx_request_header_case_insensitive_and_lowercased_map() {
            let headers = HashMap::from([("X-Flow-Id".to_string(), "flow-9".to_string())]);
            let script = r#"
                function respond(ctx) {
                    var byGetter = ctx.request.header("x-flow-id");
                    var byMap = ctx.request.headers["x-flow-id"];
                    return http(200, { getter: byGetter, map: byMap });
                }
            "#;
            let decision = run_respond(script, &req(headers, None)).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("flow-9"));
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        #[test]
        fn ctx_request_json_lazy_parse() {
            let script = r#"
                function respond(ctx) {
                    if (ctx.request.json === null) { return http(500); }
                    return http(200, { n: ctx.request.json.n });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), Some(r#"{"n": 7}"#))).unwrap();
            match decision {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains('7'));
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }

            let decision2 = run_respond(script, &req(HashMap::new(), Some("not json"))).unwrap();
            assert!(matches!(
                decision2,
                FaultDecision::Error { status: 500, .. }
            ));
        }

        #[test]
        fn result_constructors_delay_reset_pass() {
            let delay = run_respond(
                "function respond(ctx) { return delay(42); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            match delay {
                FaultDecision::Latency { duration_ms, .. } => assert_eq!(duration_ms, 42),
                other => panic!("expected Latency, got {other:?}"),
            }

            let reset = run_respond(
                "function respond(ctx) { return reset(); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(reset, FaultDecision::Reset { .. }));

            let pass = run_respond(
                "function respond(ctx) { return pass(); }",
                &req(HashMap::new(), None),
            )
            .unwrap();
            assert!(matches!(pass, FaultDecision::None));

            let nothing =
                run_respond("function respond(ctx) { }", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(nothing, FaultDecision::None));
        }

        #[test]
        fn http_string_body_passes_through_verbatim() {
            let script = r#"function respond(ctx) { return http(200, "hello world"); }"#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, headers, .. } => {
                    assert_eq!(body, "hello world");
                    assert!(!headers.contains_key("Content-Type"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }

        // Issue #453: v1 `should_inject` was removed — a script defining only `should_inject`
        // (and no `respond`) is now just a misnamed entrypoint, not a special v1 detection.
        #[test]
        fn should_inject_only_script_is_misnamed_entrypoint_error() {
            let script = r#"
                function should_inject(request, flow_store) {
                    return { inject: true, fault: "error", status: 418, body: "teapot" };
                }
            "#;
            let err = run_respond(script, &req(HashMap::new(), None)).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("entrypoint") && msg.contains("respond"),
                "expected the standard misnamed-entrypoint error naming `respond`, got: {msg}"
            );
        }

        // Retry example from the issue: ctx.state.incr + http() executes end-to-end.
        #[test]
        fn retry_example_end_to_end_with_ctx_state_incr() {
            let script = r#"
                function respond(ctx) {
                    var n = ctx.state.incr("attempts");
                    if (n <= 2) {
                        return http(503, { error: "unavailable", attempt: n }).header("Retry-After", "1");
                    }
                    return http(200, { ok: true, succeededOnAttempt: n });
                }
            "#;
            let engine = JsEngine::new(script, "retry-rule").unwrap();
            let shared_store = store();
            let request = req(HashMap::new(), None);

            let d1 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d1, FaultDecision::Error { status: 503, .. }));

            let d2 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            assert!(matches!(d2, FaultDecision::Error { status: 503, .. }));

            let d3 = engine.should_inject(&request, shared_store).unwrap();
            match d3 {
                FaultDecision::Error { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert!(body.contains("succeededOnAttempt"));
                }
                other => panic!("expected 200 on third attempt, got {other:?}"),
            }
        }

        // --- ctx.state atomic ops (issue #358) ---

        #[test]
        fn get_or_returns_default_when_absent_then_stored_value() {
            let script = r#"
                function respond(ctx) {
                    var first = ctx.state.getOr("count", 0);
                    ctx.state.set("count", 7);
                    var second = ctx.state.getOr("count", 0);
                    return http(200, { first: first, second: second });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"first\":0"), "got {body}");
                    assert!(body.contains("\"second\":7"), "got {body}");
                }
                other => panic!("expected Error(200), got {other:?}"),
            }
        }

        #[test]
        fn incr_by_is_atomic_and_starts_at_zero() {
            let script = r#"
                function respond(ctx) {
                    var n = ctx.state.incrBy("hits", 5);
                    return http(200, { n: n });
                }
            "#;
            let engine = JsEngine::new(script, "incr-by-rule").unwrap();
            let shared_store = store();
            let request = req(HashMap::new(), None);

            let d1 = engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            match d1 {
                FaultDecision::Error { body, .. } => assert!(body.contains("\"n\":5")),
                other => panic!("expected Error(200), got {other:?}"),
            }

            let d2 = engine.should_inject(&request, shared_store).unwrap();
            match d2 {
                FaultDecision::Error { body, .. } => assert!(body.contains("\"n\":10")),
                other => panic!("expected Error(200), got {other:?}"),
            }
        }

        #[test]
        fn cas_distinguishes_applied_from_conflict() {
            let shared_store = store();
            let request = req(HashMap::new(), None);

            // First call: key "status" absent, expected null matches -> applied, sets "paid".
            let applied_script = r#"
                function respond(ctx) {
                    var outcome = ctx.state.cas("status", null, "paid");
                    return http(200, { applied: outcome.applied, current: outcome.current });
                }
            "#;
            let applied_engine = JsEngine::new(applied_script, "cas-applied").unwrap();
            let d1 = applied_engine
                .should_inject(&request, Arc::clone(&shared_store))
                .unwrap();
            match d1 {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":true"), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }

            // Second call: current is now "paid"; expecting "pending" must conflict and report the
            // winning current value, distinguishing it from the Applied case above.
            let conflict_script = r#"
                function respond(ctx) {
                    var outcome = ctx.state.cas("status", "pending", "shipped");
                    return http(200, { applied: outcome.applied, current: outcome.current });
                }
            "#;
            let conflict_engine = JsEngine::new(conflict_script, "cas-conflict").unwrap();
            let d2 = conflict_engine
                .should_inject(&request, shared_store)
                .unwrap();
            match d2 {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":false"), "got {body}");
                    assert!(body.contains("\"current\":\"paid\""), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        #[test]
        fn ttl_sets_per_flow_expiry() {
            let script = r#"
                function respond(ctx) {
                    ctx.state.set("k", 1);
                    var applied = ctx.state.ttl(3600);
                    return http(200, { applied: applied });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("\"applied\":true"), "got {body}");
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // Issue #358 B2: JS has one number type, so `set("count", 5)` arrives as f64 5.0. The
        // js_to_json integer fix stores it as an integer so a following incr_by/incr accumulates
        // instead of silently restarting from 0.
        #[test]
        fn set_whole_number_then_incr_by_accumulates() {
            let script = r#"
                function respond(ctx) {
                    ctx.state.set("count", 5);
                    var a = ctx.state.incrBy("count", 1);
                    var b = ctx.state.incr("count");
                    return http(200, { a: a, b: b });
                }
            "#;
            let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(
                        body.contains("\"a\":6"),
                        "expected incr_by to yield 6, got {body}"
                    );
                    assert!(
                        body.contains("\"b\":7"),
                        "expected incr to yield 7, got {body}"
                    );
                }
                other => panic!("expected 200, got {other:?}"),
            }
        }

        // Issue #358 B3 (AC4): a backend failure on any atomic op must propagate as a script error.
        #[test]
        fn atomic_ops_propagate_store_errors_fail_loud() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            for (name, script) in [
                (
                    "get_or",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.getOr("k", 0) }); }"#,
                ),
                (
                    "incr_by",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.incrBy("k", 1) }); }"#,
                ),
                (
                    "cas",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.cas("k", null, "v").applied }); }"#,
                ),
                (
                    "ttl",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.ttl(60) }); }"#,
                ),
                (
                    "ttl_key",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.ttl("k", 60) }); }"#,
                ),
                (
                    "clear",
                    r#"function respond(ctx) { return http(200, { v: ctx.state.clear() }); }"#,
                ),
            ] {
                let result = JsEngine::new(script, "fail")
                    .unwrap()
                    .should_inject(&request, Arc::new(FailingFlowStore));
                assert!(result.is_err(), "{name} must propagate a store failure");
            }
        }

        // Issue #358 B1 / #322: the ctx.state ops (get/incr) are unconditionally fail-loud.
        #[test]
        fn state_ops_fail_loud_on_backend_failure() {
            use crate::extensions::flow_state::FailingFlowStore;
            let request = req(HashMap::new(), None);

            let get_err = JsEngine::new(
                r#"function respond(ctx) { return http(200, { v: ctx.state.get("k") }); }"#,
                "v2-get",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(get_err.is_err(), "v2 ctx.state.get must fail loud");

            let incr_err = JsEngine::new(
                r#"function respond(ctx) { return http(200, { v: ctx.state.incr("k") }); }"#,
                "v2-incr",
            )
            .unwrap()
            .should_inject(&request, Arc::new(FailingFlowStore));
            assert!(incr_err.is_err(), "v2 ctx.state.incr must fail loud");
        }

        // B1 (issue #357): a script defining ONLY a misnamed entrypoint must Err, not None.
        #[test]
        fn misnamed_entrypoint_errors_not_none() {
            let script = r#"
                function respnod(ctx) {
                    return http(500);
                }
            "#;
            let result = JsEngine::new(script, "v2-rule")
                .unwrap()
                .should_inject(&req(HashMap::new(), None), store());
            assert!(
                result.is_err(),
                "a misnamed entrypoint must surface an error, got {result:?}"
            );
        }

        #[test]
        fn genuine_bare_expression_still_ok_after_b1() {
            let decision = run_respond("http(503)", &req(HashMap::new(), None)).unwrap();
            assert!(matches!(decision, FaultDecision::Error { status: 503, .. }));
        }

        // ctx.request.pathParams and ctx.request.query round-trip (mirrors the Rhai test).
        #[test]
        fn ctx_request_path_params_and_query() {
            let mut request = req(HashMap::new(), None);
            request
                .path_params
                .insert("id".to_string(), "42".to_string());
            request
                .query
                .insert("page".to_string(), vec!["2".to_string()]);
            let script = r#"
                function respond(ctx) {
                    return http(200, { id: ctx.request.pathParams.id, page: ctx.request.query.page });
                }
            "#;
            let decision = run_respond(script, &request).unwrap();
            match decision {
                FaultDecision::Error { body, .. } => {
                    assert!(body.contains("42"), "pathParams.id missing: {body}");
                    assert!(body.contains('2'), "query.page missing: {body}");
                }
                other => panic!("expected Error(200) carrier, got {other:?}"),
            }
        }

        // B3 (issue #357): the ScriptResult registry must not grow unbounded across runs on a
        // reused worker thread. Each execution makes constructor calls that are NOT the returned
        // value (an orphaned `http(999)` completion), yet the registry is reset per run, so its
        // size stays bounded (here: empty at the start of each run, so <= 1 entry mid-run).
        #[test]
        fn script_result_registry_does_not_leak_across_runs() {
            let script = "http(999); pass()"; // orphaned http(999), returns pass()
            for _ in 0..25 {
                let decision = run_respond(script, &req(HashMap::new(), None)).unwrap();
                assert!(matches!(decision, FaultDecision::None));
            }
            let leaked = SCRIPT_RESULT_REGISTRY.with(|r| r.borrow().len());
            assert!(
                leaked <= 1,
                "registry leaked {leaked} entries across 25 runs; per-run reset is broken"
            );
        }
    }

    // =========================================================================================
    // Issue #1593: scripts get `console`, Promise jobs and timers, within the run's deadline.
    // =========================================================================================

    fn inject(script: &str) -> Result<MountebankInjectResponse> {
        execute_mountebank_inject(
            script,
            &mb_req("GET", "/1593"),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
    }

    #[test]
    fn an_async_inject_serves_its_body() {
        let resp = inject("async config => ({ body: 'asyncret' })").expect("async inject");
        assert_eq!(resp.body_text(), "asyncret");
    }

    #[test]
    fn an_async_inject_awaits_a_timer() {
        let resp = inject(
            "async config => { await new Promise(r => setTimeout(r, 5)); \
             return { body: 'awaited' }; }",
        )
        .expect("async inject");
        assert_eq!(resp.body_text(), "awaited");
    }

    #[test]
    fn a_callback_from_a_promise_job_is_honoured() {
        let resp = inject(
            "(config, state, logger, callback) => { \
               Promise.resolve('p').then(v => callback({ body: v })); }",
        )
        .expect("then-callback inject");
        assert_eq!(resp.body_text(), "p");
    }

    #[test]
    fn a_callback_from_a_timer_is_honoured_after_its_delay() {
        let started = std::time::Instant::now();
        let resp = inject(
            "(config, state, logger, callback) => { \
               setTimeout(() => callback({ body: 'late' }), 20); }",
        )
        .expect("timer-callback inject");
        assert_eq!(resp.body_text(), "late");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(20),
            "answered before its timer was due: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_rejected_inject_promise_is_an_inject_error() {
        let err = inject("async config => { throw new Error('nope'); }")
            .expect_err("a rejected promise is an error");
        let failure = err
            .downcast_ref::<InjectionFailure>()
            .unwrap_or_else(|| panic!("an InjectionFailure, got: {err:#}"));
        assert_eq!(failure.data.as_deref(), Some("nope"));
    }

    #[test]
    fn clear_timeout_cancels_a_timer() {
        let resp = inject(
            "(config, state, logger, callback) => { \
               const t = setTimeout(() => callback({ body: 'first' }), 5); \
               clearTimeout(t); \
               setTimeout(() => callback({ body: 'second' }), 10); }",
        )
        .expect("inject");
        assert_eq!(resp.body_text(), "second");
    }

    #[test]
    fn the_first_answer_wins() {
        // Mountebank resolves one promise: a callback before the return keeps its value.
        let resp = inject(
            "(config, state, logger, callback) => { \
               callback({ body: 'called' }); return { body: 'returned' }; }",
        )
        .expect("inject");
        assert_eq!(resp.body_text(), "called");
    }

    #[test]
    fn timers_that_never_call_back_leave_no_answer() {
        let err = inject("(config, state, logger, callback) => { setTimeout(() => {}, 1); }")
            .expect_err("no answer");
        assert!(err.to_string().contains("must return an object"), "{err:#}");
    }

    #[tokio::test]
    async fn an_inject_timer_past_the_deadline_is_the_timeout_error() {
        let started = std::time::Instant::now();
        let err = execute_mountebank_inject_bounded(
            "(config, state, logger, callback) => { \
               setTimeout(() => callback({ body: 'never' }), 10000); }"
                .to_string(),
            mb_req("GET", "/1593"),
            ScriptScope::detached(test_port()),
            None,
            trusted_files(),
            std::time::Duration::from_millis(200),
        )
        .await
        .expect_err("a timer past the deadline");
        assert_eq!(
            err.downcast_ref::<crate::scripting::ScriptTimeoutError>(),
            Some(&crate::scripting::ScriptTimeoutError {
                hook: "inject",
                timeout_ms: 200
            }),
            "{err:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn an_endless_promise_chain_ends_at_the_deadline() {
        let err = execute_mountebank_inject_bounded(
            "async config => { async function f() { await 0; return f(); } return f(); }"
                .to_string(),
            mb_req("GET", "/1593"),
            ScriptScope::detached(test_port()),
            None,
            trusted_files(),
            std::time::Duration::from_millis(200),
        )
        .await
        .expect_err("an endless chain never answers");
        assert!(
            err.downcast_ref::<crate::scripting::ScriptTimeoutError>()
                .is_some(),
            "{err:#}"
        );
    }

    // The worker itself gives up when the next timer is due after the deadline: it does not
    // sleep through it, so the pool thread is free again at once.
    #[test]
    fn a_worker_does_not_wait_for_a_timer_past_its_deadline() {
        let mut thread = MbJsThread::new();
        let started = std::time::Instant::now();
        let err = execute_mountebank_inject_within(
            &mut thread,
            "(config, state, logger, callback) => { setTimeout(() => callback({}), 10000); }",
            &mb_req("GET", "/1593"),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
            RunBudget::starting_now(std::time::Duration::from_millis(100)),
        )
        .expect_err("past the deadline");
        assert!(
            err.downcast_ref::<crate::scripting::ScriptTimeoutError>()
                .is_some(),
            "{err:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn console_logs_under_rift_script() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: Arc::clone(&events),
        };
        let port = test_port();
        let scope = ScriptScope::detached(port);
        let result = tracing::subscriber::with_default(subscriber, || {
            execute_mountebank_inject(
                "() => { console.log('x-1593', { a: 1 }); console.warn('w-1593'); \
                   console.error('e-1593'); console.info('i-1593'); console.debug('d-1593'); \
                   return { body: 'logged' }; }",
                &mb_req("GET", "/log"),
                scope.clone(),
                Some("stub-1593"),
                &trusted_files(),
            )
        });
        assert_eq!(
            result.expect("console must not throw").body_text(),
            "logged"
        );
        let logs = events.lock().unwrap();
        for message in ["x-1593 {\"a\":1}", "w-1593", "e-1593", "i-1593", "d-1593"] {
            assert!(
                logs.iter().any(|l| l.contains("target=rift::script")
                    && l.contains(message)
                    && l.contains(&port.to_string())
                    && l.contains("stub-1593")),
                "no rift::script event for {message}: {logs:?}"
            );
        }
    }

    #[test]
    fn scripts_get_no_node_process_globals() {
        let resp = inject(
            "() => ({ body: [typeof process, typeof Buffer, typeof require, typeof console, \
               typeof setTimeout, typeof clearTimeout, typeof setInterval, \
               typeof clearInterval].join(',') })",
        )
        .expect("inject");
        // `require` is the hook's own (issue #1611); there is no `process` or `Buffer`.
        assert_eq!(
            resp.body_text(),
            "undefined,undefined,function,object,function,function,function,function"
        );
    }

    #[test]
    fn a_decorate_gets_console_and_timers() {
        let decorated = execute_mountebank_decorate(
            "async config => { console.log('deco-1593'); \
               await new Promise(r => setTimeout(r, 5)); config.response.body = 'after-timer'; }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("decorate");
        assert_eq!(decorated.body, "after-timer");
    }

    // Issue #1679: an `async` decorate that settles to a falsy value mutated the response, as a
    // synchronous one that returns nothing does. Mountebank answers 500 (`undefined`) or an empty
    // response (`0`) instead; documented in `differences.md`, kept on purpose.
    #[test]
    fn an_async_decorate_resolving_to_a_falsy_value_is_a_mutation() {
        for script in [
            "async config => { config.response.body = 'm'; }",
            "async config => { config.response.body = 'm'; return 0; }",
        ] {
            let decorated = execute_mountebank_decorate(
                script,
                &config_req(),
                "orig",
                &crate::behaviors::ResponseStatus::Code(201),
                &crate::util::OrderedMap::default(),
                &ScriptScope::detached(test_port()),
                None,
                &trusted_files(),
            )
            .expect("decorate");
            assert_eq!(decorated.body, "m", "{script}");
        }
    }

    #[test]
    fn a_decorate_timer_past_its_deadline_is_the_timeout_error() {
        let started = std::time::Instant::now();
        let err = execute_mountebank_decorate_within(
            "async config => { await new Promise(r => setTimeout(r, 10000)); }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
            std::time::Duration::from_millis(100),
        )
        .expect_err("past the deadline");
        assert_eq!(
            err.downcast_ref::<crate::scripting::ScriptTimeoutError>(),
            Some(&crate::scripting::ScriptTimeoutError {
                hook: "decorate",
                timeout_ms: 100
            }),
            "{err:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    // A decorate that returns no promise is not waited for, as in Mountebank: its timer never
    // runs, and the response is what the function left.
    #[test]
    fn a_sync_decorate_is_not_waited_for() {
        let decorated = execute_mountebank_decorate(
            "config => { setTimeout(() => { config.response.body = 'late'; }, 1); \
               config.response.body = 'now'; }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("decorate");
        assert_eq!(decorated.body, "now");
    }

    #[test]
    fn a_predicate_inject_gets_console_and_timers() {
        let matched = execute_predicate_inject(
            "config => { console.log('pred-1593'); setTimeout(() => {}, 1); return true; }",
            &mb_req("GET", "/p"),
            ScriptScope::detached(test_port()),
            &trusted_files(),
        )
        .expect("predicate inject");
        assert!(matched);
    }

    // Issue #1645 for timers: a timer a run leaves pending is discarded before the realm's next
    // run, so it never fires in a later run, whichever imposter that run belongs to.
    #[test]
    fn a_leftover_timer_never_fires_in_a_later_run() {
        let mut thread = MbJsThread::new();
        let refused = Arc::new(ScriptFileAccess::Refused);
        execute_mountebank_inject_in(
            &mut thread,
            "config => { setTimeout(() => { globalThis.__timer1593 = 'fired'; }, 0); \
             return { body: 'queued' }; }",
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &refused,
        )
        .expect("queue");
        let read = execute_mountebank_inject_in(
            &mut thread,
            "(config, state, logger, callback) => { \
               setTimeout(() => callback({ body: String(globalThis.__timer1593) }), 5); }",
            &mb_req("GET", "/b"),
            &ScriptScope::detached(test_port()),
            None,
            &refused,
        )
        .expect("read");
        assert_eq!(read.body_text(), "undefined");
    }

    // Issue #1645 for timers: a confined imposter's timer that would `require` a file outside
    // its root never runs in a trusted imposter's run on the same worker, even one that waits on
    // timers of its own.
    #[test]
    fn a_pending_timer_never_runs_under_another_imposters_require() {
        let tree = scripts_tree();
        let mut thread = MbJsThread::new();
        let confined = Arc::new(ScriptFileAccess::Confined(tree.root.clone()));
        let trusted = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        let plant = format!(
            "config => {{ setTimeout(() => {{ \
               try {{ globalThis.__x1593 = String(require({:?})); }} \
               catch (e) {{ globalThis.__x1593 = 'ERR:' + String(e); }} }}, 0); \
             return {{ body: 'planted' }}; }}",
            tree.outside.join("mod.js").to_str().unwrap()
        );
        execute_mountebank_inject_in(
            &mut thread,
            &plant,
            &mb_req("GET", "/a"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("plant");
        let decorated = execute_mountebank_decorate_in(
            &mut thread,
            "async config => { await new Promise(r => setTimeout(r, 5)); \
               config.response.body = String(globalThis.__x1593); }",
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted,
        )
        .expect("trusted async decorate");
        assert_eq!(decorated.body, "undefined");
        let read = execute_mountebank_inject_in(
            &mut thread,
            "(config, state, logger, callback) => { \
               setTimeout(() => callback({ body: String(globalThis.__x1593) }), 5); }",
            &mb_req("GET", "/b"),
            &ScriptScope::detached(test_port()),
            None,
            &confined,
        )
        .expect("read");
        let body = read.body.expect("a body");
        assert!(!body.contains("CANARY_1611"), "{body}");
        assert_eq!(body, "undefined");
    }

    // Review of #1593: no script context has `Atomics.waitAsync`, whose waiter would leak its
    // buffer in Boa's process-wide futex list once the run drops it. `Atomics.wait` stays, and
    // throws, as no context may block.
    #[test]
    fn no_script_context_has_atomics_wait_async() {
        let probe = "[typeof Atomics.waitAsync, typeof Atomics.wait, (() => { \
            try { Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1); return 'ran'; } \
            catch (e) { return 'threw'; } })()].join(',')";
        let expected = "undefined,function,threw";
        let resp = inject(&format!("() => ({{ body: {probe} }})")).expect("inject");
        assert_eq!(resp.body_text(), expected, "an MB pool realm");
        let decorated = execute_mountebank_decorate(
            &format!("config => {{ config.response.body = {probe}; }}"),
            &config_req(),
            "orig",
            &crate::behaviors::ResponseStatus::Code(200),
            &crate::util::OrderedMap::default(),
            &ScriptScope::detached(test_port()),
            None,
            &trusted_files(),
        )
        .expect("decorate");
        assert_eq!(decorated.body, expected, "a decorate realm");
        for (name, mut context) in [
            ("sealed", sealed_js_context()),
            ("bounded", bounded_js_context()),
        ] {
            let value = context
                .eval(Source::from_bytes(probe.as_bytes()))
                .expect("probe")
                .to_string(&mut context)
                .expect("string")
                .to_std_string_escaped();
            assert_eq!(value, expected, "the {name} context");
        }
    }

    // ---- Issue #1678: the runs of one realm interleave on their worker. ----

    /// Start an inject on `thread` without driving it; its answer arrives once it has one.
    fn start_inject_on(
        thread: &mut MbJsThread,
        inject: &str,
        scope: &ScriptScope,
        files: &Arc<ScriptFileAccess>,
    ) -> std::sync::mpsc::Receiver<Result<MountebankInjectResponse>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        start_inject(
            thread,
            InjectInput::new(inject, &mb_req("GET", "/1678"), scope.clone(), None, files),
            RunBudget::default_now(),
            Reply::Sync(tx),
            Claim::local(),
        );
        rx
    }

    /// [`start_inject_on`] within `budget`.
    fn start_inject_within_on(
        thread: &mut MbJsThread,
        inject: &str,
        scope: &ScriptScope,
        budget: RunBudget,
    ) -> std::sync::mpsc::Receiver<Result<MountebankInjectResponse>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        start_inject(
            thread,
            InjectInput::new(
                inject,
                &mb_req("GET", "/1678"),
                scope.clone(),
                None,
                &trusted_files(),
            ),
            budget,
            Reply::Sync(tx),
            Claim::local(),
        );
        rx
    }

    fn answer_body(rx: &std::sync::mpsc::Receiver<Result<MountebankInjectResponse>>) -> String {
        rx.try_recv()
            .expect("the run answered")
            .expect("the inject")
            .body_text()
            .to_string()
    }

    fn owned(key: StateKey) -> bool {
        mb_js_pool().routes.lock().owners.contains_key(&key)
    }

    #[test]
    fn runs_of_one_imposter_interleave_over_one_live_state() {
        let mut thread = MbJsThread::new();
        let scope = ScriptScope::detached(test_port());
        let slow = start_inject_on(
            &mut thread,
            "async config => { config.state.order = 'a'; \
             await new Promise(r => setTimeout(r, 50)); \
             config.state.order += 'c'; return { body: config.state.order }; }",
            &scope,
            &trusted_files(),
        );
        let quick = start_inject_on(
            &mut thread,
            "config => { config.state.order += 'b'; return { body: config.state.order }; }",
            &scope,
            &trusted_files(),
        );
        // The quick run answered while the slow one waited, saw its write, and the slow one saw
        // the quick one's at its next await.
        assert_eq!(answer_body(&quick), "ab");
        thread.drive();
        assert_eq!(answer_body(&slow), "abc");
        assert_eq!(scope.state.lock().get("order"), Some(&json!("abc")));
    }

    // A run that throws keeps the writes it made before, in the live object its imposter's runs
    // share and so in the cell, as Mountebank's `imposterState` does.
    #[test]
    fn a_throwing_runs_state_writes_persist() {
        let scope = ScriptScope::detached(test_port());
        let err = execute_mountebank_inject(
            "config => { config.state.seen = true; throw new Error('boom-1678'); }",
            &mb_req("GET", "/1678"),
            scope.clone(),
            None,
            &trusted_files(),
        )
        .expect_err("it throws");
        assert!(err.to_string().contains("boom-1678"), "{err:#}");
        assert_eq!(scope.state.lock().get("seen"), Some(&json!(true)));
    }

    #[test]
    fn a_job_that_throws_fails_only_its_own_run() {
        let mut thread = MbJsThread::new();
        let thrower = start_inject_on(
            &mut thread,
            "config => new Promise(() => setTimeout(() => { throw new Error('tick-1678'); }, 10))",
            &ScriptScope::detached(test_port()),
            &trusted_files(),
        );
        let waiter = start_inject_on(
            &mut thread,
            "config => new Promise(r => setTimeout(() => r({ body: 'fine' }), 30))",
            &ScriptScope::detached(test_port()),
            &trusted_files(),
        );
        thread.drive();
        let err = thrower
            .try_recv()
            .expect("answered")
            .expect_err("its timer threw");
        assert!(err.to_string().contains("tick-1678"), "{err:#}");
        assert_eq!(answer_body(&waiter), "fine");
    }

    // A `require` a run keeps answers in that run's own timer, even after another run of the realm
    // ran in between, and never in the other run.
    #[test]
    fn a_kept_require_answers_only_in_its_own_run() {
        let tree = scripts_tree();
        let trusted = Arc::new(ScriptFileAccess::Trusted {
            base: tree.root.clone(),
        });
        let mut thread = MbJsThread::new();
        let scope = ScriptScope::detached(test_port());
        let keeper = start_inject_on(
            &mut thread,
            "async config => { globalThis.__kept1678 = require; \
             await new Promise(r => setTimeout(r, 30)); \
             return { body: String(require('top.js')) }; }",
            &scope,
            &trusted,
        );
        let borrower = start_inject_on(
            &mut thread,
            "config => { try { return { body: String(__kept1678('top.js')) }; } \
             catch (e) { return { body: 'ERR:' + String(e) }; } }",
            &scope,
            &trusted,
        );
        let borrowed = answer_body(&borrower);
        assert!(
            borrowed.starts_with("ERR:") && borrowed.contains("another run"),
            "{borrowed}"
        );
        thread.drive();
        assert_eq!(answer_body(&keeper), "A>B:true");
    }

    #[test]
    fn console_in_a_timer_logs_as_its_own_run() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: Arc::clone(&events),
        };
        tracing::subscriber::with_default(subscriber, || {
            let mut thread = MbJsThread::new();
            let a = start_inject_on(
                &mut thread,
                "config => new Promise(r => setTimeout(() => { console.log('from-a-1678'); \
                 r({ body: 'a' }); }, 30))",
                &ScriptScope::detached(41_678),
                &trusted_files(),
            );
            let b = start_inject_on(
                &mut thread,
                "config => new Promise(r => setTimeout(() => { console.log('from-b-1678'); \
                 r({ body: 'b' }); }, 10))",
                &ScriptScope::detached(42_678),
                &trusted_files(),
            );
            thread.drive();
            assert_eq!((answer_body(&a), answer_body(&b)), ("a".into(), "b".into()));
        });
        let logs = events.lock().unwrap();
        let line = |message: &str| {
            logs.iter()
                .find(|l| l.contains(message))
                .cloned()
                .unwrap_or_else(|| panic!("no event for {message}: {logs:?}"))
        };
        let (a, b) = (line("from-a-1678"), line("from-b-1678"));
        assert!(a.contains("41678") && !a.contains("42678"), "{a}");
        assert!(b.contains("42678") && !b.contains("41678"), "{b}");
    }

    // A restart swaps the imposter's cell (issue #1726), which is a new live state: a run still in
    // flight finishes on the old one and saves into the old cell; the next run starts fresh.
    #[test]
    fn a_new_cell_starts_fresh_while_the_old_ones_runs_finish() {
        let mut thread = MbJsThread::new();
        let old = ScriptScope::detached(test_port());
        let parked = start_inject_on(
            &mut thread,
            "async config => { config.state.n = 1; await new Promise(r => setTimeout(r, 30)); \
             config.state.n += 1; return { body: String(config.state.n) }; }",
            &old,
            &trusted_files(),
        );
        let fresh = ScriptScope::detached(old.port);
        let read = start_inject_on(
            &mut thread,
            "config => ({ body: String(config.state.n) })",
            &fresh,
            &trusted_files(),
        );
        assert_eq!(answer_body(&read), "undefined");
        thread.drive();
        assert_eq!(answer_body(&parked), "2");
        assert_eq!(old.state.lock().get("n"), Some(&json!(2)));
        assert!(fresh.state.lock().is_empty());
    }

    #[test]
    fn a_realm_with_a_run_in_flight_is_never_dropped() {
        let mut thread = MbJsThread::new();
        let dir = tempfile::tempdir().expect("a directory");
        let parked = start_inject_on(
            &mut thread,
            "config => new Promise(r => setTimeout(() => r({ body: 'kept' }), 30))",
            &ScriptScope::detached(test_port()),
            &trusted_files(),
        );
        for i in 0..MB_REALMS_PER_WORKER + 2 {
            let access = Arc::new(ScriptFileAccess::Confined(dir.path().join(i.to_string())));
            let other = start_inject_on(
                &mut thread,
                "config => ({ body: 'other' })",
                &ScriptScope::detached(test_port()),
                &access,
            );
            assert_eq!(answer_body(&other), "other");
        }
        thread.drive();
        assert_eq!(answer_body(&parked), "kept");
    }

    /// A pool of `workers` with no threads behind it, for the routing rules alone.
    fn routing_pool(workers: usize) -> &'static MbPool {
        Box::leak(Box::new(MbPool {
            queues: (0..workers)
                .map(|_| crossbeam::channel::unbounded().0)
                .collect(),
            shared: crossbeam::channel::unbounded().0,
            busy: (0..workers)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            queued: (0..workers)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            routes: parking_lot::Mutex::new(Routes {
                load: vec![0; workers],
                owners: std::collections::HashMap::new(),
            }),
        }))
    }

    #[test]
    fn every_claim_of_an_imposter_goes_to_one_worker_until_the_last_is_released() {
        let pool = routing_pool(2);
        // Held for the test, as a run holds its scope: a key is unique only while its cell lives.
        let scopes: Vec<_> = (0..4).map(|_| ScriptScope::detached(test_port())).collect();
        let key = state_key(&scopes[0]);
        let claims: Vec<Claim> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| s.spawn(|| pool.claim(Some(key)).expect("a claim")))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("claim thread"))
                .collect()
        });
        assert!(claims.iter().all(|claim| claim.worker() == Some(0)));
        assert_eq!(
            pool.routes
                .lock()
                .owners
                .get(&key)
                .map(|owner| owner.claims),
            Some(16)
        );
        // Another imposter goes to the worker with fewer claims...
        let second = pool.claim(Some(state_key(&scopes[1]))).expect("a claim");
        assert_eq!(second.worker(), Some(1));
        // ...unless that one is busy executing, or has a job it has not started: an idle worker
        // with an empty queue comes first.
        pool.busy[1].store(true, std::sync::atomic::Ordering::Relaxed);
        let third = pool.claim(Some(state_key(&scopes[2]))).expect("a claim");
        assert_eq!(third.worker(), Some(0));
        pool.busy[1].store(false, std::sync::atomic::Ordering::Relaxed);
        pool.queued[1].store(1, std::sync::atomic::Ordering::Relaxed);
        let fourth = pool.claim(Some(state_key(&scopes[3]))).expect("a claim");
        assert_eq!(fourth.worker(), Some(0));
        drop(fourth);
        drop(claims);
        assert!(!pool.routes.lock().owners.contains_key(&key));
        drop((second, third));
        assert_eq!(pool.routes.lock().load, [0, 0]);
    }

    // A run whose caller stopped waiting is dropped long before its deadline: its interval stops
    // firing, and its worker place is released. That bounds a run that never answers, whatever
    // script timeout its imposter sets.
    #[tokio::test]
    async fn an_abandoned_run_is_dropped_before_its_deadline() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let _capture = tracing::subscriber::set_default(CapturingSubscriber {
            events: Arc::clone(&events),
        });
        let ticks = || {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains("tick-1678"))
                .count()
        };
        let scope = ScriptScope::detached(test_port());
        let key = state_key(&scope);
        let caller = tokio::spawn(execute_mountebank_inject_bounded(
            "config => { setInterval(() => console.log('tick-1678'), 5); }".to_string(),
            mb_req("GET", "/1678"),
            scope.clone(),
            None,
            trusted_files(),
            std::time::Duration::from_secs(60),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(owned(key), "precondition: the run is in flight");
        assert!(ticks() > 0, "precondition: its interval fires");
        caller.abort();
        let started = std::time::Instant::now();
        while owned(key) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1),
                "the abandoned run is still in flight"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let at_drop = ticks();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(ticks(), at_drop, "its interval fired after it was dropped");
    }

    // Two runs of one imposter can share a promise through `state`, as in Mountebank: the one
    // waiting on the other's promise is not ended for want of jobs of its own, and the reaction it
    // registered still runs after the run that settled the promise has answered.
    #[test]
    fn runs_of_one_imposter_can_share_a_promise_through_state() {
        let mut thread = MbJsThread::new();
        let scope = ScriptScope::detached(test_port());
        let maker = start_inject_on(
            &mut thread,
            "config => { config.state.p = new Promise(r => setTimeout(() => r('shared'), 20)); \
             return config.state.p.then(v => ({ body: 'maker:' + v })); }",
            &scope,
            &trusted_files(),
        );
        let waiter = start_inject_on(
            &mut thread,
            "config => config.state.p.then(v => ({ body: 'waiter:' + v }))",
            &scope,
            &trusted_files(),
        );
        thread.drive();
        assert_eq!(answer_body(&maker), "maker:shared");
        assert_eq!(answer_body(&waiter), "waiter:shared");
    }

    // An imposter's script file access can change while it runs (an admin stub write narrows it),
    // which puts its next run in another realm: that run waits for the runs in the old realm, so
    // one live state object holds the imposter's state at a time and no update is lost.
    #[test]
    fn a_run_waits_while_its_imposter_has_runs_in_another_realm() {
        let mut thread = MbJsThread::new();
        let scope = ScriptScope::detached(test_port());
        let first = start_inject_on(
            &mut thread,
            "async config => { const n = config.state.n || 0; \
             await new Promise(r => setTimeout(r, 30)); \
             config.state.n = n + 1; return { body: String(config.state.n) }; }",
            &scope,
            &trusted_files(),
        );
        let second = start_inject_on(
            &mut thread,
            "config => { config.state.n = (config.state.n || 0) + 1; \
             return { body: String(config.state.n) }; }",
            &scope,
            &Arc::new(ScriptFileAccess::Refused),
        );
        assert!(second.try_recv().is_err(), "the second run waits");
        thread.drive();
        assert_eq!(answer_body(&first), "1");
        assert_eq!(answer_body(&second), "2");
        assert_eq!(scope.state.lock().get("n"), Some(&json!(2)));
    }

    // A worker runs a bounded number of one run's jobs before it starts the requests queued for it:
    // an endless chain of promise jobs does not hold up its imposter's other requests.
    #[tokio::test]
    async fn an_endless_promise_chain_does_not_hold_up_its_imposters_next_run() {
        let scope = ScriptScope::detached(test_port());
        let chain = tokio::spawn(execute_mountebank_inject_bounded(
            "async config => { async function f() { await 0; return f(); } return f(); }"
                .to_string(),
            mb_req("GET", "/1678"),
            scope.clone(),
            None,
            trusted_files(),
            std::time::Duration::from_secs(5),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let quick = execute_mountebank_inject_bounded(
            "config => ({ body: 'quick' })".to_string(),
            mb_req("GET", "/1678"),
            scope.clone(),
            None,
            trusted_files(),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the quick run answers");
        let took = started.elapsed();
        chain.abort();
        assert_eq!(quick.body_text(), "quick");
        assert!(
            took < std::time::Duration::from_secs(1),
            "the quick run waited {took:?} behind the chain"
        );
    }

    // What a run leaves queued when it answers may pass to another run of its imposter, but never
    // past its own deadline: an endless chain it started dies then, rather than living on run
    // after run while the imposter has traffic (and starving the timers of every run it reaches).
    #[test]
    fn a_passed_on_chain_ends_at_the_deadline_of_the_run_that_started_it() {
        let mut thread = MbJsThread::new();
        let scope = ScriptScope::detached(test_port());
        let waiter = start_inject_on(
            &mut thread,
            "config => new Promise(r => setTimeout(() => r({ body: 'timer ran' }), 50))",
            &scope,
            &trusted_files(),
        );
        let spinner = start_inject_within_on(
            &mut thread,
            "config => { (async function f() { await 0; f(); })(); return { body: 'spinning' }; }",
            &scope,
            RunBudget::starting_now(std::time::Duration::from_millis(100)),
        );
        assert_eq!(answer_body(&spinner), "spinning");
        let started = std::time::Instant::now();
        thread.drive();
        assert_eq!(answer_body(&waiter), "timer ran");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the passed-on chain outlived its run's deadline: {:?}",
            started.elapsed()
        );
    }
}
