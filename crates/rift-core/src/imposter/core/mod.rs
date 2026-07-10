//! Core Imposter struct and implementation.
//!
//! This module contains the Imposter struct which represents a single
//! running imposter instance with its configuration, stubs, and state.

/// Default scenario state when a `(flow_id, scenario)` entry is absent (WireMock parity).
pub const INITIAL_SCENARIO_STATE: &str = "Started";

use super::predicates::stub_matches_inner;
use super::response::{
    create_response_preview, create_stub_from_proxy_response, execute_stub_response_with_rift,
    get_rift_script_config,
};
use super::types::{
    DebugImposter, DebugResponsePreview, DebugStubInfo, ImposterConfig, ImposterError,
    ProxyResponse, RecordedRequest, ResponseMode, RiftResponseExtension, RiftScriptConfig, Stub,
    StubResponse,
};
use crate::backends::InMemoryFlowStore;
use crate::behaviors::{HasRepeatBehavior, RuleCycler};
use crate::extensions::flow_state::{FlowStore, NoOpFlowStore};
use crate::recording::{
    ClaimOutcome, LocalProxyStore, ProxyMode, ProxyRecordingStore, RecordedResponse,
    RequestSignature,
};
use anyhow::Context;
use arc_swap::{ArcSwap, ArcSwapOption};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Maximum allowed proxy response body size (10 MB)
const MAX_PROXY_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Request timeout for the shared proxy HTTP client.
const PROXY_HTTP_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default time-to-live for the fallback in-memory flow store when a stub declares
/// scenario state but no explicit flow-state TTL is configured.
const DEFAULT_FLOW_STATE_TTL_SECS: u64 = 300;

/// Global HTTP client for proxy requests
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROXY_HTTP_CLIENT_TIMEOUT)
            // Keep connection pooling on (issue #482): every proxied request otherwise paid a
            // fresh TCP + TLS handshake. Staleness is bounded by reqwest's default
            // `pool_idle_timeout` (90s) — the standard reqwest pooling tradeoff.
            .build()
            .expect("Failed to create HTTP client: check system TLS/DNS configuration")
    })
}

/// Process-wide slot mint: globally unique tokens are trivially per-imposter unique, and a
/// global counter avoids threading imposter context into every construction site.
static NEXT_STUB_SLOT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct StubState {
    pub(crate) stub: Stub,
    cycler: Arc<RuleCycler>,
    /// Slot token for sequencer keying (issue #313): minted at insertion, preserved by
    /// in-place replaces (which keep the StubState), dropped with the slot.
    pub(crate) slot: u64,
}

impl StubState {
    #[must_use]
    pub fn new(stub: Stub) -> Self {
        Self {
            stub,
            cycler: Arc::new(RuleCycler::new()),
            slot: NEXT_STUB_SLOT.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn get_next_response(&self) -> Option<&StubResponse> {
        let responses = &self.stub.responses;
        if responses.is_empty() {
            return None;
        }

        let repeat_for_response = |idx| responses.get(idx as usize).and_then(|r| r.get_repeat());
        let response_idx = self
            .cycler
            .get_response_index_advance(responses.len() as u32, repeat_for_response);
        responses.get(response_idx as usize)
    }

    #[must_use]
    pub fn peek_response(&self) -> Option<&StubResponse> {
        let responses = &self.stub.responses;
        let response_idx = self.cycler.peek_response_index(responses.len() as u32);
        self.stub.responses.get(response_idx as usize)
    }

    /// A copy carrying `stub` but preserving this slot's response-cycling state (the shared
    /// `cycler`) and `slot`. Used by the reconcile/replace paths to swap stub content in place
    /// now that states live behind `Arc` and cannot be mutated through a shared reference.
    #[must_use]
    pub(crate) fn with_stub(&self, stub: Stub) -> Self {
        Self {
            stub,
            cycler: Arc::clone(&self.cycler),
            slot: self.slot,
        }
    }
}

/// Runtime state of an imposter
pub struct Imposter {
    pub config: ImposterConfig,
    /// Mutable stubs (can be modified at runtime). Stored behind `Arc` so a matched request
    /// takes a refcount bump instead of deep-cloning the whole `StubState` (issue #287), and
    /// behind `ArcSwap` so the match hot path reads wait-free (`load()`, no lock) while a
    /// mutation atomically swaps in a fresh snapshot (issue #291). Serialize *writers* through
    /// [`Self::mutate_stubs`] / `stubs_write`; never `store()` this field directly.
    pub stubs: ArcSwap<Vec<Arc<StubState>>>,
    /// Stage-1 path-anchor prefilter over the current `stubs` snapshot (issue #292). Rebuilt in
    /// [`Self::mutate_stubs`] alongside `stubs`; the match hot path loads it wait-free and uses the
    /// snapshot it embeds for both candidate selection and evaluation (so no torn read).
    stub_index: ArcSwap<StubIndex>,
    /// Serializes stub *writers* so a read-copy-update (clone snapshot → mutate → store) can't
    /// lose a concurrent update (issue #291). Off the request hot path — held only by admin /
    /// reload / proxy-record mutations, never by readers.
    stubs_write: Mutex<()>,
    /// Proxy-recording backend (issue #315); defaults to a private port-scoped
    /// [`LocalProxyStore`] for this imposter's mode, or the embedder's shared store injected
    /// via [`ImposterManager::with_proxy_store`](crate::imposter::ImposterManager::with_proxy_store).
    pub(crate) proxy_store: Arc<dyn ProxyRecordingStore>,
    /// Recorded-request storage (issue #314); defaults to a private LocalJournal,
    /// or the embedder's shared journal injected via the manager.
    pub(crate) journal: Arc<dyn crate::imposter::journal::RequestJournal>,
    /// Whether imposter is enabled
    pub enabled: AtomicBool,
    /// Creation timestamp (for future metrics/admin display)
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Shutdown signal sender (for future graceful shutdown)
    pub shutdown_tx: Option<broadcast::Sender<()>>,
    /// Flow store for Rift extensions (stateful scripting)
    pub flow_store: Arc<dyn FlowStore>,
    /// Pluggable response-cursor backend (issue #313); None = embedded per-stub cycler.
    pub(crate) sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
    /// Cached stub-overlap analysis warnings (issue #423). `None` = dirty; recomputed lazily on the
    /// next [`Self::stub_warnings`] read and reused until the next `mutate_stubs`. Keeps the O(n)
    /// analysis off the per-`GET` hot path and gives HTTP and embedded/FFI one shared code path.
    stub_warnings: ArcSwapOption<Vec<crate::extensions::stub_analysis::StubWarning>>,
}

impl Imposter {
    /// Create a new imposter from config (no custom flow-store provider).
    pub fn new(config: ImposterConfig) -> anyhow::Result<Self> {
        Self::new_with_provider(config, None)
    }

    /// Create a new imposter, consulting `provider` for its flow store before the built-in
    /// `_rift.flowState` selection (issue #312).
    pub fn new_with_provider(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_hooks(config, provider, None)
    }

    /// Create a new imposter with all embedder hooks: the flow-store `provider` (#312) and
    /// a pluggable response `sequencer` (#313; None = embedded per-stub cycler).
    pub fn new_with_hooks(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_hooks_and_journal(config, provider, sequencer, None)
    }

    /// Create a new imposter with all embedder hooks, including the request journal (#314).
    ///
    /// `config.port` must be resolved (Some) before the imposter serves requests: a shared
    /// journal keys by port, and unresolved ports would silently multiplex onto slot 0.
    ///
    /// Fails (issue #325) when an explicitly-requested `_rift.flowState.backend` cannot be
    /// built (e.g. `"redis"` with no config block, or without the `redis-backend` feature) —
    /// such requests must not silently downgrade to `NoOpFlowStore`. The implicit NoOp (no
    /// `_rift.flowState` configured) still succeeds.
    pub fn new_with_hooks_and_journal(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
        journal: Option<Arc<dyn crate::imposter::journal::RequestJournal>>,
    ) -> anyhow::Result<Self> {
        let stubs: Vec<Arc<StubState>> = config
            .stubs
            .iter()
            .map(|stub| Arc::new(StubState::new(stub.clone())))
            .collect();
        // Share the exact snapshot with the Stage-1 index so a reader that loads the index gets a
        // self-consistent (stubs, candidates) pair (issue #292).
        let stubs_arc = Arc::new(stubs);
        let stub_index = ArcSwap::from_pointee(StubIndex::build(Arc::clone(&stubs_arc)));

        // Extract proxy mode from stubs (use first proxy response's mode)
        let proxy_mode = Self::extract_proxy_mode(&config.stubs);

        // Initialize flow store: a registered provider wins; otherwise the built-in
        // `_rift.flowState` selection.
        let flow_store = Self::create_flow_store(&config, provider)?;

        Ok(Self {
            config,
            stubs: ArcSwap::new(stubs_arc),
            stub_index,
            stubs_write: Mutex::new(()),
            proxy_store: Arc::new(LocalProxyStore::new(proxy_mode)),
            journal: journal
                .unwrap_or_else(|| Arc::new(crate::imposter::journal::LocalJournal::default())),
            enabled: AtomicBool::new(true),
            created_at: chrono::Utc::now(),
            shutdown_tx: None,
            flow_store,
            sequencer,
            stub_warnings: ArcSwapOption::empty(),
        })
    }

    /// Read-copy-update the stub vector (issue #291). Reads stay wait-free (`self.stubs.load()`);
    /// a mutation takes the `stubs_write` mutex (serializing writers so no update is lost), clones
    /// the current snapshot, applies `f`, then atomically swaps the new snapshot in. `f`'s return
    /// value is passed back to the caller. Mutations are off the request hot path.
    ///
    /// The new snapshot is stored unconditionally — even when `f` reports a no-op via its return
    /// value (e.g. an out-of-bounds `Err` or a duplicate-id `false`). That is safe only because
    /// every such `f` validates *before* touching the vector, so a rejected call stores a
    /// content-identical clone. Any `f` added here must uphold that: never partially mutate the
    /// vector and then return an error/no-op, or the partial change would be committed silently.
    pub(crate) fn mutate_stubs<R>(&self, f: impl FnOnce(&mut Vec<Arc<StubState>>) -> R) -> R {
        let _writer = self.stubs_write.lock();
        let mut next: Vec<Arc<StubState>> = self.stubs.load_full().as_ref().clone();
        let result = f(&mut next);
        // Store the snapshot and rebuild the Stage-1 index from the *same* Arc, under the write
        // lock, so the two never diverge (issue #292).
        let next_arc = Arc::new(next);
        self.stubs.store(Arc::clone(&next_arc));
        self.stub_index.store(Arc::new(StubIndex::build(next_arc)));
        // Invalidate the cached stub-analysis warnings (issue #423). This is O(1) — the actual
        // O(n) recompute is deferred to the next `stub_warnings()` read — so high-frequency
        // mutations (e.g. proxy recording) don't pay analysis cost on every recorded stub.
        self.stub_warnings.store(None);
        result
    }

    /// The imposter's stub-overlap analysis warnings (issue #423), computed once and cached until
    /// the next stub mutation. The HTTP `GET /imposters/:port` handler and embedded/FFI consumers
    /// both read this, so analysis runs off the per-read hot path and is identical for standalone
    /// and embedded instances (which previously got no analysis at all).
    pub fn stub_warnings(&self) -> Arc<Vec<crate::extensions::stub_analysis::StubWarning>> {
        // Fast path: a warm cache is a wait-free load, so the request/read hot path never blocks.
        if let Some(cached) = self.stub_warnings.load_full() {
            return cached;
        }
        // Miss: compute-and-store under the same `stubs_write` lock `mutate_stubs` holds, so a
        // mutation's invalidation can never be lost by a slow reader that computed over a stale
        // snapshot (a store racing an invalidation would pin stale warnings). The lock is taken
        // only on a miss — once per mutation — and the O(n) analysis is off the request hot path.
        let _writer = self.stubs_write.lock();
        // Re-check under the lock: another reader may have populated it while we waited.
        if let Some(cached) = self.stub_warnings.load_full() {
            return cached;
        }
        let warnings = crate::extensions::stub_analysis::analyze_stubs(&self.get_stubs()).warnings;
        let arc = Arc::new(warnings);
        self.stub_warnings.store(Some(Arc::clone(&arc)));
        arc
    }

    /// Replace all stubs
    pub fn replace_stubs(&self, new_stubs: Vec<Stub>) {
        self.mutate_stubs(|stubs| {
            stubs.clear();
            stubs.extend(new_stubs.into_iter().map(|s| Arc::new(StubState::new(s))));
        });
    }

    /// Create flow store based on _rift.flowState configuration.
    /// Falls back to a default in-memory store (not NoOp) when stubs declare the scenario FSM or
    /// carry a `_rift.script` that might call `ctx.state` (issue #358), so both work out of the
    /// box without explicit `_rift.flowState`. Only an imposter with neither surface gets the
    /// silent `NoOpFlowStore` — such an imposter never touches the store anyway.
    ///
    /// Note: the store is chosen at construction. Scenario/script stubs added later via an
    /// in-place `PUT /imposters/:port/stubs` to an imposter that started with neither (and no
    /// `_rift.flowState`) will hit the NoOp store and not persist — declare them at creation,
    /// configure `_rift.flowState`, use `PUT /imposters` (which recreates), or set a
    /// manager-scoped `FlowStoreProvider` returning a shared store (issue #312).
    fn create_flow_store(
        config: &ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
    ) -> anyhow::Result<Arc<dyn FlowStore>> {
        if let Some(provider) = provider
            && let Some(store) = provider.provide(config)
        {
            // A provider store wins over any built-in selection, including an explicit
            // `_rift.flowState` — log it so an operator whose config was overridden can see why.
            if config
                .rift
                .as_ref()
                .and_then(|r| r.flow_state.as_ref())
                .is_some()
            {
                debug!(
                    "FlowStoreProvider supplied a store, overriding the imposter's _rift.flowState"
                );
            } else {
                debug!("FlowStoreProvider supplied the imposter flow store");
            }
            return Ok(store);
        }
        if let Some(flow_state_config) = config.rift.as_ref().and_then(|r| r.flow_state.as_ref()) {
            return match flow_state_config.backend.as_str() {
                "inmemory" => {
                    info!(
                        "Creating InMemory FlowStore for imposter (ttl={}s)",
                        flow_state_config.ttl_seconds
                    );
                    Ok(Arc::new(InMemoryFlowStore::new(
                        flow_state_config.ttl_seconds as u64,
                    )))
                }
                "redis" => Self::create_redis_flow_store(flow_state_config),
                #[cfg(feature = "test-backend")]
                "failing" => {
                    info!("Creating deliberately failing FlowStore (test-backend feature)");
                    Ok(Arc::new(crate::extensions::flow_state::FailingFlowStore))
                }
                // An explicitly-set but unrecognized backend is a config error, not a reason to
                // silently downgrade to NoOp (issue #377) — fail construction like the redis arm.
                other => anyhow::bail!(
                    "flowState.backend is \"{other}\" but no such backend exists (expected \"inmemory\" or \"redis\")"
                ),
            };
        }

        if Self::uses_scenario_fsm(&config.stubs) {
            info!("Stubs declare scenario state; using default in-memory FlowStore");
            return Ok(Arc::new(InMemoryFlowStore::new(
                DEFAULT_FLOW_STATE_TTL_SECS,
            )));
        }

        if Self::uses_script(&config.stubs) {
            // A script may call `ctx.state`/`flow_store` at runtime — not visible statically — so
            // provision a real store rather than let it silently discard writes (issue #358). This
            // is a convenience default, not a production recommendation: warn so an operator who
            // didn't mean to rely on it notices in their logs.
            tracing::warn!(
                target: "rift::script",
                ttl_seconds = DEFAULT_FLOW_STATE_TTL_SECS,
                "imposter has a _rift.script stub but no _rift.flowState configured; \
                 auto-provisioning an in-memory FlowStore. State will NOT persist across restarts \
                 or be shared across a cluster — configure _rift.flowState for production."
            );
            return Ok(Arc::new(InMemoryFlowStore::new(
                DEFAULT_FLOW_STATE_TTL_SECS,
            )));
        }

        Ok(Arc::new(NoOpFlowStore))
    }

    /// Whether any stub references a scenario — via the declarative FSM fields
    /// (`requiredScenarioState`/`newScenarioState`) or by merely naming one (`scenarioName`, #514).
    fn uses_scenario_fsm(stubs: &[Stub]) -> bool {
        // A stub that merely names a scenario (`scenarioName`, no gate/transition) still needs a
        // real flow store (issue #514): the admin/FFI scenario surface reads and writes the store
        // directly, outside request matching, so landing on the NoOp store makes set-state/reset a
        // silent no-op. Triggering on `scenario_name` too is a strict superset of the FSM fields.
        stubs.iter().any(|s| {
            s.scenario_name.is_some()
                || s.required_scenario_state.is_some()
                || s.new_scenario_state.is_some()
        })
    }

    /// Whether any stub can execute a Rift script (`_rift.script` on an `Is` response, or the
    /// script-only `RiftScript` response) — such a stub might call `ctx.state`/`flow_store` at
    /// runtime, so its imposter needs a real flow store even without `_rift.flowState`
    /// configured (issue #358). Conservative like [`Self::uses_tcp_faults`]: a `RiftScript`
    /// response always counts, since whether it actually touches the store isn't visible here.
    fn uses_script(stubs: &[Stub]) -> bool {
        stubs
            .iter()
            .flat_map(|s| &s.responses)
            .any(|resp| match resp {
                StubResponse::Is { rift: Some(r), .. } => r.script.is_some(),
                StubResponse::RiftScript { .. } => true,
                _ => false,
            })
    }

    /// Whether any current stub can trigger a connection-level TCP fault (`_rift.fault.tcp`, or a
    /// Mountebank `fault` that maps to one). Such imposters must be served HTTP/1-only: a TCP fault
    /// aborts the whole socket, which is incompatible with HTTP/2 stream multiplexing (issue #295).
    ///
    /// A `_rift.script` (RiftScript) stub also counts (issue #357 B2): a script can dynamically
    /// call `reset()` — the v2 connection-reset result constructor — which lowers to the same
    /// transport-level abort as `_rift.fault.tcp`, so a script-bearing stub must likewise be
    /// served HTTP/1-only even though its reset path isn't visible in the static config.
    pub(crate) fn uses_tcp_faults(&self) -> bool {
        use crate::imposter::fault_io::TcpFaultKind;
        let rift_has_tcp = |rift: &RiftResponseExtension| {
            rift.fault
                .as_ref()
                .and_then(|f| f.tcp.as_ref())
                .is_some_and(|t| TcpFaultKind::parse(t).is_some())
        };
        self.stubs
            .load()
            .iter()
            .flat_map(|s| &s.stub.responses)
            .any(|resp| match resp {
                StubResponse::Fault { fault } => TcpFaultKind::parse(fault).is_some(),
                StubResponse::Is { rift: Some(r), .. } => rift_has_tcp(r),
                // A script may call `reset()` at runtime, so conservatively force H1-only.
                StubResponse::RiftScript { .. } => true,
                _ => false,
            })
    }

    /// The key under which this imposter's Mountebank inject/decorate script state is stored in
    /// the process-global `IMPOSTER_STATE` map — the imposter's bound listener port.
    ///
    /// `config.port` is `Some(bound_port)` for the entire life of a live imposter: the manager
    /// assigns and records the real, distinct bound port in `create_imposter_inner` *before* the
    /// imposter is constructed or serves a request, so distinct imposters (including auto-bind
    /// ones) always get distinct keys and never clobber each other's script state (issue #439).
    /// The `unwrap_or(0)` fallback is therefore unreachable for a live imposter — `0` is the
    /// documented "no live imposter" sentinel (see `rift-ffi`); it exists only to keep this
    /// total. Centralised here so the invariant has one greppable home rather than a scattered
    /// magic `config.port.unwrap_or(0)` at every script call site.
    pub(crate) fn script_state_key(&self) -> u16 {
        self.config.port.unwrap_or(0)
    }

    /// Create Redis flow store if configured and available.
    ///
    /// An explicitly-requested `"redis"` backend that cannot be built must fail imposter
    /// construction rather than silently downgrade to `NoOpFlowStore` (issue #325).
    #[allow(unused_variables)]
    fn create_redis_flow_store(
        flow_state_config: &crate::imposter::types::RiftFlowStateConfig,
    ) -> anyhow::Result<Arc<dyn FlowStore>> {
        #[cfg(feature = "redis-backend")]
        {
            let Some(ref redis_config) = flow_state_config.redis else {
                error!("Redis backend selected but no redis config provided");
                anyhow::bail!(
                    "flowState.backend is \"redis\" but no redis config block was provided"
                );
            };

            use crate::backends::RedisFlowStore;
            match RedisFlowStore::new(
                &redis_config.url,
                redis_config.pool_size,
                redis_config.key_prefix.clone(),
                flow_state_config.ttl_seconds,
            ) {
                Ok(store) => {
                    info!(
                        "Created Redis FlowStore for imposter (url={}, ttl={}s)",
                        redis_config.url, flow_state_config.ttl_seconds
                    );
                    Ok(Arc::new(store))
                }
                Err(e) => {
                    error!("Failed to create Redis FlowStore: {}", e);
                    anyhow::bail!("failed to build the Redis flow store: {e}");
                }
            }
        }

        #[cfg(not(feature = "redis-backend"))]
        {
            error!("Redis backend not available (compile with --features redis-backend)");
            anyhow::bail!(
                "flowState.backend is \"redis\" but this binary was built without the redis-backend feature (rebuild with --features redis-backend)"
            );
        }
    }

    /// Extract proxy mode from stubs
    fn extract_proxy_mode(stubs: &[Stub]) -> ProxyMode {
        for stub in stubs {
            for response in &stub.responses {
                if let StubResponse::Proxy { proxy } = response {
                    return match proxy.mode.to_lowercase().as_str() {
                        "proxyonce" => ProxyMode::ProxyOnce,
                        "proxyalways" => ProxyMode::ProxyAlways,
                        "proxytransparent" | "" => ProxyMode::ProxyTransparent,
                        _ => ProxyMode::ProxyTransparent,
                    };
                }
            }
        }
        ProxyMode::ProxyTransparent
    }
}

mod lifecycle;
mod matching;
mod stub_index;
use stub_index::StubIndex;
mod proxy;
mod recording;
mod responses;
mod verify;
pub use verify::{ClosestMatch, FailedPredicate, VerifyOptions, VerifyOutcome};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::types::ImposterConfig;
    use serde_json::json;

    // Issue #482: removing `pool_max_idle_per_host(0)` (re-enabling connection pooling) must keep
    // the client builder valid — a bad builder config would panic in the `.expect` on first use.
    #[test]
    fn proxy_http_client_builds() {
        let _client = get_http_client();
    }

    fn make_test_imposter() -> Imposter {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            ..Default::default()
        };
        Imposter::new(config).expect("test imposter")
    }

    // Issue #514: a stub declaring only `scenarioName` (no requiredScenarioState/newScenarioState)
    // is a valid Mountebank config — it names a scenario without gating on it. It must still get a
    // real flow store, or the admin/FFI scenario surface (set-state/list/reset) silently no-ops: the
    // write vanishes into the NoOp store and a read still shows the initial state.
    #[test]
    fn bare_scenario_name_provisions_a_real_flow_store() {
        let cfg: ImposterConfig = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{
                "scenarioName": "order",
                "responses": [{ "is": { "statusCode": 200 } }]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let flow = imp.resolve_flow_id(&std::collections::HashMap::new());

        imp.set_scenario_state(&flow, "order", "paid")
            .expect("set scenario state");
        assert_eq!(
            imp.scenario_state(&flow, "order")
                .expect("read scenario state"),
            "paid",
            "a bare-scenarioName stub must persist scenario state (real flow store, not NoOp)"
        );
    }

    // Issue #423: stub-analysis warnings are computed once and cached — repeated reads return the
    // SAME Arc (no per-read recompute) — and a stub mutation invalidates the cache so the next read
    // reflects the new stubs.
    #[test]
    fn stub_warnings_cached_until_mutation() {
        use crate::extensions::stub_analysis::WarningType;

        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/dup" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] },
                { "predicates": [{ "equals": { "path": "/dup" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        let a = imp.stub_warnings();
        assert!(
            a.iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate),
            "duplicate stubs must be flagged"
        );
        let b = imp.stub_warnings();
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "repeated reads must return the cached Arc — no recompute on the hot path"
        );

        // Mutating the stubs invalidates the cache; the next read recomputes over the new stubs.
        let distinct: Vec<Stub> = serde_json::from_value(json!([
            { "predicates": [{ "equals": { "path": "/only" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "y" } }] }
        ]))
        .unwrap();
        imp.replace_stubs(distinct);

        let c = imp.stub_warnings();
        assert!(
            !std::sync::Arc::ptr_eq(&a, &c),
            "a stub mutation must invalidate the cached warnings"
        );
        assert!(
            !c.iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate),
            "distinct stubs must have no exact-duplicate warning"
        );
    }

    #[test]
    fn matched_stub_is_shared_arc_not_deep_cloned() {
        // Gate for #287: a match returns the Arc<StubState> stored in the stub vector (a refcount
        // bump), not a deep clone — proven by pointer identity with the stored entry.
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/shared" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let headers = std::collections::HashMap::new();
        let (matched, index) = imp
            .find_matching_stub_with_client("GET", "/shared", &headers, None, None, None, None)
            .expect("store is infallible")
            .expect("request must match");
        let stored = std::sync::Arc::clone(&imp.stubs.load()[index]);
        assert!(
            std::sync::Arc::ptr_eq(&matched, &stored),
            "matched stub must be the shared Arc, not a deep clone"
        );
    }

    // Issue #325: an explicitly-requested redis backend that can't be built must fail imposter
    // construction loudly, not silently downgrade to NoOp. Constructed at the ctor level (no port
    // bind needed). `redis-backend` is a default feature, so the first case runs in `cargo test`.
    #[cfg(feature = "redis-backend")]
    #[test]
    fn explicit_redis_without_config_block_fails_construction() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "redis" } }
        }))
        .expect("valid imposter config");
        let result = Imposter::new_with_hooks_and_journal(cfg, None, None, None);
        assert!(
            result.is_err(),
            "redis backend requested with no redis config block must fail construction, not NoOp"
        );
    }

    #[cfg(not(feature = "redis-backend"))]
    #[test]
    fn explicit_redis_without_feature_fails_construction() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "redis", "redis": { "url": "redis://localhost:6379" } } }
        }))
        .expect("valid imposter config");
        let result = Imposter::new_with_hooks_and_journal(cfg, None, None, None);
        assert!(
            result.is_err(),
            "redis backend without the redis-backend feature must fail construction, not NoOp"
        );
    }

    // Issue #325/#358: an unreachable redis URL must fail imposter creation loudly (via
    // `RedisFlowStore::new`'s eager PING), not silently downgrade to NoOp/in-memory.
    #[cfg(feature = "redis-backend")]
    #[test]
    fn redis_unreachable_url_fails_construction() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "redis", "redis": { "url": "redis://127.0.0.1:1" } } }
        }))
        .expect("valid imposter config");
        let result = Imposter::new_with_hooks_and_journal(cfg, None, None, None);
        assert!(
            result.is_err(),
            "an unreachable redis URL must fail imposter construction, not NoOp"
        );
    }

    // Issue #358: a `_rift.script` stub might call `ctx.state`/`flow_store` at runtime — without
    // `_rift.flowState` configured it must get a REAL in-memory store, not the silent NoOp (state
    // must persist across calls; NoOp's `increment` always returns 1).
    #[test]
    fn script_stub_without_flow_state_auto_provisions_in_memory() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200 },
                    "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { pass() }" } }
                }]
            }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(
            imp.flow_store.increment("f", "attempts").expect("incr"),
            2,
            "NoOpFlowStore would return 1 both times; the auto-provisioned in-memory store must persist"
        );
    }

    // Same guarantee for the script-only `RiftScript` response variant (no `is` wrapper).
    #[test]
    fn script_only_response_without_flow_state_auto_provisions_in_memory() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { http(200) }" } }
                }]
            }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 2);
    }

    // An imposter with neither a scenario stub nor a script stub keeps the silent NoOp — it never
    // touches the store, so provisioning a real one would be pure waste.
    #[test]
    fn plain_stub_without_flow_state_stays_noop() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        // NoOpFlowStore.increment always returns 1 — it never persists.
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
    }

    #[test]
    fn implicit_noop_construction_succeeds_silently() {
        // No _rift.flowState and no scenario stubs: the implicit NoOp store must not be an error.
        let cfg = serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": [] }))
            .expect("valid imposter config");
        assert!(Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_ok());
    }

    #[test]
    fn inmemory_backend_construction_succeeds() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "inmemory" } }
        }))
        .expect("valid imposter config");
        assert!(Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_ok());
    }

    // Issue #377: an explicitly-set but unrecognized backend (a typo) must fail construction, not
    // silently downgrade to NoOp — the same fail-loud contract #325 gave the redis arm.
    #[test]
    fn explicit_unknown_backend_fails_construction() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "backend": "postgres" } }
        }))
        .expect("valid imposter config");
        assert!(
            Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_err(),
            "an unrecognized flowState.backend must fail construction, not fall back to NoOp"
        );
    }

    /// Serve the state's next response body, advancing the shared cycler.
    fn served_body(state: &StubState) -> String {
        let resp = state.get_next_response().expect("stub has responses");
        serde_json::to_value(resp).expect("serialize")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    #[test]
    fn replace_stub_preserves_response_cursor() {
        // Gate for #287: the index-based in-place replace reuses the slot's cycler (via
        // `with_stub`), so the response cursor survives a content swap rather than resetting.
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.stubs.load()[0]), "A"); // cursor now at index 1
        let new = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "C" } },
                { "is": { "statusCode": 200, "body": "D" } }
            ]
        }))
        .unwrap();
        imp.replace_stub(0, new).expect("index in bounds");
        assert_eq!(
            served_body(&imp.stubs.load()[0]),
            "D",
            "cursor (index 1) preserved and content swapped"
        );
    }

    #[test]
    fn replace_stub_by_id_preserves_response_cursor() {
        // Same guarantee for the id-based in-place replace (#287).
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "id": "s1",
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.stubs.load()[0]), "A"); // cursor now at index 1
        let new = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "C" } },
                { "is": { "statusCode": 200, "body": "D" } }
            ]
        }))
        .unwrap();
        assert!(imp.replace_stub_by_id("s1", new), "id exists");
        assert_eq!(
            served_body(&imp.stubs.load()[0]),
            "D",
            "cursor (index 1) preserved and content swapped"
        );
    }

    #[test]
    fn proxy_always_append_preserves_cursor_and_slot() {
        // Gate for #287: the proxyAlways append branch rebuilds the entry via `with_stub`, so
        // the appended-to stub keeps its slot token and response cursor rather than getting a
        // fresh StubState.
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": { "to": "http://upstream", "mode": "proxyAlways" } }] }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        // First record → inserted after the proxy stub (insert branch), 2 responses.
        let rec = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/rec" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "R1" } },
                { "is": { "statusCode": 200, "body": "R2" } }
            ]
        }))
        .unwrap();
        imp.insert_or_append_proxy_stub(rec, "http://upstream", "proxyAlways");

        let rec_index = imp
            .stubs
            .load()
            .iter()
            .position(|s| !s.stub.predicates.is_empty())
            .expect("recorded stub present");
        let slot_before = imp.stubs.load()[rec_index].slot;
        assert_eq!(served_body(&imp.stubs.load()[rec_index]), "R1"); // cursor now at index 1

        // Second record with identical predicates → append branch: responses become [R1, R2, R3].
        let rec2 = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/rec" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "R3" } }]
        }))
        .unwrap();
        imp.insert_or_append_proxy_stub(rec2, "http://upstream", "proxyAlways");

        assert_eq!(
            imp.stubs.load()[rec_index].slot,
            slot_before,
            "append must reuse the slot token, not mint a fresh StubState"
        );
        assert_eq!(
            served_body(&imp.stubs.load()[rec_index]),
            "R2",
            "cursor (index 1) preserved across the append"
        );
    }

    #[test]
    fn concurrent_writers_no_lost_update() {
        // Gate for #291: writers RCU under a serializing mutex, so N concurrent `add_stub` calls
        // all land. A naive load→clone→mutate→store without serialization would lose updates
        // (two writers clone the same snapshot; the second store clobbers the first).
        use std::sync::Arc as StdArc;
        let imp = StdArc::new(make_test_imposter());
        let n = 32usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let imp = StdArc::clone(&imp);
                std::thread::spawn(move || {
                    let stub: Stub = serde_json::from_value(json!({
                        "id": format!("s{i}"),
                        "responses": [{ "is": { "statusCode": 200, "body": "x" } }]
                    }))
                    .unwrap();
                    imp.add_stub(stub, None);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            imp.get_stubs().len(),
            n,
            "every concurrent add must be retained (no lost update)"
        );
    }

    #[test]
    fn cursor_survives_unrelated_structural_mutation() {
        // Gate for #291: an unrelated structural mutation (adding another stub) rebuilds the whole
        // stub-vector snapshot. The untouched stub's `Arc<StubState>` must be carried into the new
        // snapshot by reference (Arc clone), so its response cursor is preserved — a deep copy of
        // the state would reset it.
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.stubs.load()[0]), "A"); // cursor now at index 1
        let other: Stub = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/other" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "Z" } }]
        }))
        .unwrap();
        imp.add_stub(other, None);
        assert_eq!(
            served_body(&imp.stubs.load()[0]),
            "B",
            "cursor preserved across an unrelated structural snapshot swap (Arc identity carried)"
        );
    }

    #[test]
    fn concurrent_reads_during_swap_consistent() {
        // Gate for #291: readers on the match path load a consistent, wait-free snapshot while a
        // writer swaps the entire stub set. The path always has a matching stub, so the reader
        // must keep matching and must never tear or panic.
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "predicates": [{ "equals": { "path": "/p" } }],
                        "responses": [{ "is": { "statusCode": 200, "body": "v0" } }] }]
        }))
        .unwrap();
        let imp = StdArc::new(Imposter::new(cfg).expect("test imposter"));
        let stop = StdArc::new(AtomicBool::new(false));
        let reader = {
            let imp = StdArc::clone(&imp);
            let stop = StdArc::clone(&stop);
            std::thread::spawn(move || {
                let headers = std::collections::HashMap::new();
                let mut hits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let r = imp
                        .find_matching_stub_with_client(
                            "GET", "/p", &headers, None, None, None, None,
                        )
                        .expect("store infallible");
                    if r.is_some() {
                        hits += 1;
                    }
                }
                hits
            })
        };
        for i in 1..500u32 {
            let stub: Stub = serde_json::from_value(json!({
                "predicates": [{ "equals": { "path": "/p" } }],
                "responses": [{ "is": { "statusCode": 200, "body": format!("v{i}") } }]
            }))
            .unwrap();
            imp.replace_stubs(vec![stub]);
        }
        stop.store(true, Ordering::Release);
        let hits = reader.join().unwrap();
        assert!(
            hits > 0,
            "reader saw a consistent matching snapshot throughout the swaps"
        );
    }

    // Fix #95: Multi-valued form fields are now comma-joined instead of overwritten
    #[test]
    fn test_parse_form_data_multi_valued_fields() {
        // parse_form_data now reads the pre-built header map (Title-Case keys); the Content-Type
        // lookup must remain case-insensitive.
        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );

        let result = Imposter::parse_form_data(&headers, Some("checkbox=A&checkbox=B&checkbox=C"));
        let form = result.expect("Should parse form data");
        assert_eq!(
            form.get("checkbox").unwrap(),
            "A,B,C",
            "Multi-valued form fields should be comma-joined"
        );

        // Content-Type lookup must be case-insensitive over the map (a lower-cased key must
        // still be recognized), matching the old case-insensitive HeaderMap::get.
        let mut lower: HashMap<String, String> = HashMap::new();
        lower.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        assert!(
            Imposter::parse_form_data(&lower, Some("a=1")).is_some(),
            "a differently-cased content-type key must still be recognized"
        );
    }

    #[test]
    fn find_matching_stub_with_client_matches_on_prebuilt_header_map() {
        // Gate for #288: the matcher takes the already-built header HashMap (no re-conversion
        // from HeaderMap) and header predicates still match against it correctly.
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "headers": { "X-Api-Key": "secret" } } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert("X-Api-Key".to_string(), "secret".to_string());
        let matched = imp
            .find_matching_stub_with_client("GET", "/", &headers, None, None, None, None)
            .expect("store is infallible");
        assert!(matched.is_some(), "request with matching header must match");

        let mut wrong: HashMap<String, String> = HashMap::new();
        wrong.insert("X-Api-Key".to_string(), "nope".to_string());
        let unmatched = imp
            .find_matching_stub_with_client("GET", "/", &wrong, None, None, None, None)
            .expect("store is infallible");
        assert!(
            unmatched.is_none(),
            "request with non-matching header must not match"
        );
    }

    // Fix #103: caseSensitive is now always written to generated predicate JSON,
    // so the matcher sees the generator's intended value.
    #[test]
    fn test_generator_always_writes_case_sensitive() {
        let imposter = make_test_imposter();

        let generators = vec![json!({
            "matches": { "method": true, "path": true }
            // no caseSensitive field → generator defaults to true
        })];

        let headers = HashMap::new();
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/API/Users",
                &headers,
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let pred_json = &predicates[0];

        // caseSensitive should now always be written
        assert_eq!(
            pred_json.get("caseSensitive"),
            Some(&serde_json::Value::Bool(true)),
            "Generator should always write caseSensitive to the predicate JSON"
        );

        // Deserialize and verify the matcher sees the correct value
        let pred: crate::imposter::types::Predicate = serde_json::from_value(pred_json.clone())
            .expect("Generated predicate should deserialize");

        assert_eq!(
            pred.parameters.case_sensitive,
            Some(true),
            "Matcher should see caseSensitive=true from the generated predicate"
        );
    }

    // Fix: `except` is now applied to method in generate_predicates_from_request
    #[test]
    fn test_generator_except_applied_to_method() {
        let imposter = make_test_imposter();

        let generators = vec![json!({
            "matches": { "method": true },
            "except": "^POST$"
        })];

        let headers = HashMap::new();
        let predicates = imposter
            .generate_predicates_from_request(&generators, "POST", "/test", &headers, None, None)
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let pred_json = &predicates[0];
        let method_val = pred_json["equals"]["method"].as_str().unwrap();

        assert_eq!(
            method_val, "",
            "except pattern should be applied to method in predicate generator"
        );
    }

    // Issue #481: the `except` regex for the path site now resolves via the shared cache;
    // this pins that the strip still applies (the method site alone left path/body untested).
    #[test]
    fn test_generator_except_applied_to_path() {
        let imposter = make_test_imposter();

        let generators = vec![json!({
            "matches": { "path": true },
            "except": r"\d+$"
        })];

        let headers = HashMap::new();
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/orders/123",
                &headers,
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let path_val = predicates[0]["equals"]["path"].as_str().unwrap();
        assert_eq!(
            path_val, "/orders/",
            "except pattern should strip the trailing digits from the path"
        );
    }

    // Issue #481: same for the body except site.
    #[test]
    fn test_generator_except_applied_to_body() {
        let imposter = make_test_imposter();

        let generators = vec![json!({
            "matches": { "body": true },
            "except": r"\d+"
        })];

        let headers = HashMap::new();
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "POST",
                "/test",
                &headers,
                Some("token=abc123"),
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let body_val = predicates[0]["equals"]["body"].as_str().unwrap();
        assert_eq!(
            body_val, "token=abc",
            "except pattern should strip digits from the body"
        );
    }

    // Fix #109: generate_predicates_from_request now handles query parameters
    #[test]
    fn test_generator_includes_query_parameters() {
        let imposter = make_test_imposter();

        let generators = vec![json!({
            "matches": { "path": true, "query": true }
        })];

        let headers = HashMap::new();
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/search",
                &headers,
                None,
                Some("q=hello&page=1"),
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let pred_json = &predicates[0];
        let equals_obj = pred_json["equals"].as_object().unwrap();

        assert!(
            equals_obj.contains_key("path"),
            "Path should be in generated predicate"
        );

        assert!(
            equals_obj.contains_key("query"),
            "Query should be in generated predicate"
        );

        let query_obj = equals_obj["query"].as_object().unwrap();
        assert_eq!(query_obj["q"].as_str().unwrap(), "hello");
        assert_eq!(query_obj["page"].as_str().unwrap(), "1");
    }

    // =========================================================================
    // Gap 5.2: predicateGenerators.inject — JS function produces predicates
    // =========================================================================

    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_produces_predicates() {
        let imposter = make_test_imposter();

        let inject_fn = r#"function(config, logger, predicates) {
            return [{ equals: { path: config.request.path } }];
        }"#;

        let generators = vec![json!({ "inject": inject_fn })];
        let headers = HashMap::new();

        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let equals = predicates[0].get("equals").expect("should have equals key");
        assert_eq!(equals["path"], "/api/users");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_receives_existing_predicates() {
        let imposter = make_test_imposter();

        // First generator builds a "method" predicate via matches, second via inject
        let inject_fn = r#"function(config, logger, predicates) {
            var result = predicates.slice();
            result.push({ equals: { path: config.request.path } });
            return result;
        }"#;

        let generators = vec![
            json!({ "matches": { "method": true } }),
            json!({ "inject": inject_fn }),
        ];
        let headers = HashMap::new();

        let predicates = imposter
            .generate_predicates_from_request(&generators, "POST", "/orders", &headers, None, None)
            .expect("predicate generation succeeds");

        // matches generator produces 1, inject generator returns 2 (original + new path)
        assert_eq!(predicates.len(), 3);
    }

    // Issue #498: a failing inject generator must be DISTINGUISHABLE from one that legitimately
    // produced no predicates. A script error surfaces as `Err` (so the caller can skip auto-stub
    // creation), while an inject that returns `[]` is `Ok(empty)` (a real, intended empty list).
    #[cfg(feature = "javascript")]
    #[test]
    fn generator_inject_failure_is_distinguishable_from_empty() {
        let imposter = make_test_imposter();
        let headers = HashMap::new();

        // A throwing inject → Err, NOT an empty predicate list.
        let throwing = r#"function(config, logger, predicates) { throw new Error("boom"); }"#;
        let err = imposter
            .generate_predicates_from_request(
                &[json!({ "inject": throwing })],
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect_err("a throwing generator must fail, not silently return empty predicates");
        assert_eq!(err.kind(), "script-error");

        // An inject that explicitly returns [] is a legitimate empty result → Ok(empty).
        let empty = r#"function(config, logger, predicates) { return []; }"#;
        let preds = imposter
            .generate_predicates_from_request(
                &[json!({ "inject": empty })],
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect("an inject returning [] is a valid empty result, not a failure");
        assert!(preds.is_empty());
    }

    #[test]
    fn test_record_request_cap_enforced() {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            record_requests: true,
            ..Default::default()
        };
        let imposter = Imposter::new(config).expect("test imposter");
        let req = RecordedRequest {
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };

        use crate::imposter::journal::MAX_RECORDED_REQUESTS;
        for _ in 0..MAX_RECORDED_REQUESTS + 10 {
            imposter.record_request(&req);
        }

        let recorded = imposter.get_recorded_requests();
        assert_eq!(
            recorded.len(),
            MAX_RECORDED_REQUESTS,
            "Recorded requests must not exceed the cap"
        );
    }

    // Issue #201: a targeted retain removes only the non-kept entries and, unlike a
    // full clear, must NOT reset the total request count (other slices' requests remain).
    #[test]
    fn test_retain_recorded_requests_preserves_count() {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            record_requests: true,
            ..Default::default()
        };
        let imposter = Imposter::new(config).expect("test imposter");

        let req = |space: &str| {
            let mut headers = std::collections::HashMap::new();
            headers.insert("X-Mock-Space".to_string(), vec![space.to_string()]);
            RecordedRequest {
                request_from: "127.0.0.1".to_string(),
                method: "GET".to_string(),
                path: "/".to_string(),
                query: std::collections::HashMap::new(),
                headers,
                body: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            }
        };
        imposter.record_request(&req("A"));
        imposter.record_request(&req("B"));
        imposter.record_request(&req("A"));
        for _ in 0..3 {
            imposter.increment_request_count();
        }

        imposter.retain_recorded_requests(|r| {
            r.headers
                .get("X-Mock-Space")
                .and_then(|v| v.first())
                .map(String::as_str)
                != Some("A")
        });
        assert_eq!(imposter.get_recorded_requests().len(), 1, "only B kept");
        assert_eq!(
            imposter.get_request_count(),
            3,
            "targeted retain must not reset the request count"
        );

        imposter.clear_recorded_requests().expect("clear");
        assert_eq!(imposter.get_recorded_requests().len(), 0);
        assert_eq!(
            imposter.get_request_count(),
            0,
            "full clear resets the request count"
        );
    }

    // Issue #498: a malformed inject generator (here, a non-function body) must FAIL rather than
    // silently return an empty predicate list — the empty list is what produced the match-all stub.
    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_bad_function_returns_err() {
        let imposter = make_test_imposter();

        let generators = vec![json!({ "inject": "not a function" })];
        let headers = HashMap::new();

        let err = imposter
            .generate_predicates_from_request(&generators, "GET", "/test", &headers, None, None)
            .expect_err("a malformed inject generator must fail, not return empty predicates");
        assert_eq!(err.kind(), "script-error");
    }
}
