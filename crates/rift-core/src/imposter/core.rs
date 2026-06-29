//! Core Imposter struct and implementation.
//!
//! This module contains the Imposter struct which represents a single
//! running imposter instance with its configuration, stubs, and state.

/// Maximum number of requests retained in memory per imposter when recording is enabled.
/// Once the cap is reached, the oldest entry is evicted (ring-buffer semantics).
const MAX_RECORDED_REQUESTS: usize = 10_000;

/// Default scenario state when a `(flow_id, scenario)` entry is absent (WireMock parity).
pub const INITIAL_SCENARIO_STATE: &str = "Started";

use super::predicates::stub_matches;
use super::response::{
    create_response_preview, create_stub_from_proxy_response, execute_stub_response_with_rift,
    get_rift_script_config,
};
use super::types::{
    DebugImposter, DebugResponsePreview, DebugStubInfo, ImposterConfig, ProxyResponse,
    RecordedRequest, ResponseMode, RiftResponseExtension, RiftScriptConfig, Stub, StubResponse,
};
use crate::backends::InMemoryFlowStore;
use crate::behaviors::{HasRepeatBehavior, RuleCycler};
use crate::extensions::flow_state::{FlowStore, NoOpFlowStore};
use crate::recording::{ProxyMode, RecordedResponse, RecordingStore, RequestSignature};
use anyhow::Context;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Maximum allowed proxy response body size (10 MB)
const MAX_PROXY_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Global HTTP client for proxy requests
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(0) // Disable connection pooling to avoid stale connections
            .build()
            .expect("Failed to create HTTP client: check system TLS/DNS configuration")
    })
}

#[derive(Debug, Clone)]
pub struct StubState {
    pub(crate) stub: Stub,
    cycler: Arc<RuleCycler>,
}

impl StubState {
    #[must_use]
    pub fn new(stub: Stub) -> Self {
        Self {
            stub,
            cycler: Arc::new(RuleCycler::new()),
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
}

/// Runtime state of an imposter
pub struct Imposter {
    pub config: ImposterConfig,
    /// Mutable stubs (can be modified at runtime)
    pub stubs: RwLock<Vec<StubState>>,
    /// Recording store for proxy responses (for future proxy mode support)
    pub recording_store: Arc<RecordingStore>,
    /// Recorded requests (if record_requests is true)
    pub recorded_requests: RwLock<Vec<RecordedRequest>>,
    /// Request count
    pub request_count: AtomicU64,
    /// Whether imposter is enabled
    pub enabled: AtomicBool,
    /// Creation timestamp (for future metrics/admin display)
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Shutdown signal sender (for future graceful shutdown)
    pub shutdown_tx: Option<broadcast::Sender<()>>,
    /// Flow store for Rift extensions (stateful scripting)
    pub flow_store: Arc<dyn FlowStore>,
}

impl Imposter {
    /// Create a new imposter from config
    pub fn new(config: ImposterConfig) -> Self {
        let stubs: Vec<StubState> = config
            .stubs
            .iter()
            .map(|stub| StubState::new(stub.clone()))
            .collect();

        // Extract proxy mode from stubs (use first proxy response's mode)
        let proxy_mode = Self::extract_proxy_mode(&config.stubs);

        // Initialize flow store based on _rift.flowState configuration
        let flow_store = Self::create_flow_store(&config);

        Self {
            config,
            stubs: RwLock::new(stubs),
            recording_store: Arc::new(RecordingStore::new(proxy_mode)),
            recorded_requests: RwLock::new(Vec::new()),
            request_count: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
            created_at: chrono::Utc::now(),
            shutdown_tx: None,
            flow_store,
        }
    }

    /// Replace all stubs
    pub fn replace_stubs(&self, new_stubs: Vec<Stub>) {
        let mut stubs = self.stubs.write();
        stubs.clear();
        stubs.extend(new_stubs.into_iter().map(StubState::new));
    }

    /// Create flow store based on _rift.flowState configuration.
    /// Falls back to a default in-memory store (not NoOp) when stubs declare the scenario FSM,
    /// so declarative scenarios work out of the box without explicit `_rift.flowState`.
    ///
    /// Note: the store is chosen at construction. Scenario stubs added later via an in-place
    /// `PUT /imposters/:port/stubs` to an imposter that started with no scenario stubs (and no
    /// `_rift.flowState`) will hit the NoOp store and not advance — declare scenario stubs at
    /// creation, configure `_rift.flowState`, or use `PUT /imposters` (which recreates).
    fn create_flow_store(config: &ImposterConfig) -> Arc<dyn FlowStore> {
        if let Some(flow_state_config) = config.rift.as_ref().and_then(|r| r.flow_state.as_ref()) {
            return match flow_state_config.backend.as_str() {
                "inmemory" => {
                    info!(
                        "Creating InMemory FlowStore for imposter (ttl={}s)",
                        flow_state_config.ttl_seconds
                    );
                    Arc::new(InMemoryFlowStore::new(flow_state_config.ttl_seconds as u64))
                }
                "redis" => Self::create_redis_flow_store(flow_state_config),
                other => {
                    warn!("Unknown flow state backend '{}', using NoOp", other);
                    Arc::new(NoOpFlowStore)
                }
            };
        }

        if Self::uses_scenario_fsm(&config.stubs) {
            info!("Stubs declare scenario state; using default in-memory FlowStore");
            return Arc::new(InMemoryFlowStore::new(300));
        }

        Arc::new(NoOpFlowStore)
    }

    /// Whether any stub declares the declarative scenario FSM (`requiredScenarioState`/`newScenarioState`).
    fn uses_scenario_fsm(stubs: &[Stub]) -> bool {
        stubs
            .iter()
            .any(|s| s.required_scenario_state.is_some() || s.new_scenario_state.is_some())
    }

    /// Create Redis flow store if configured and available
    #[allow(unused_variables)]
    fn create_redis_flow_store(
        flow_state_config: &crate::imposter::types::RiftFlowStateConfig,
    ) -> Arc<dyn FlowStore> {
        #[cfg(feature = "redis-backend")]
        {
            let Some(ref redis_config) = flow_state_config.redis else {
                error!("Redis backend selected but no redis config provided, falling back to NoOp");
                return Arc::new(NoOpFlowStore);
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
                    Arc::new(store)
                }
                Err(e) => {
                    error!(
                        "Failed to create Redis FlowStore: {}, falling back to NoOp",
                        e
                    );
                    Arc::new(NoOpFlowStore)
                }
            }
        }

        #[cfg(not(feature = "redis-backend"))]
        {
            error!("Redis backend not available (compile with --features redis-backend), falling back to NoOp");
            Arc::new(NoOpFlowStore)
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

    /// Find a matching stub for a request and return a cloned copy with its index
    pub fn find_matching_stub(
        &self,
        method: &str,
        path: &str,
        headers: &hyper::HeaderMap,
        query: Option<&str>,
        body: Option<&str>,
    ) -> Option<(StubState, usize)> {
        // Call the extended version with no client info (backward compatible)
        self.find_matching_stub_with_client(method, path, headers, query, body, None, None)
    }

    /// Find a matching stub with client address information (for requestFrom/ip predicates)
    #[allow(clippy::too_many_arguments)]
    pub fn find_matching_stub_with_client(
        &self,
        method: &str,
        path: &str,
        headers: &hyper::HeaderMap,
        query: Option<&str>,
        body: Option<&str>,
        request_from: Option<&str>,
        client_ip: Option<&str>,
    ) -> Option<(StubState, usize)> {
        let stubs = self.stubs.read();
        let headers_map = Self::header_map_to_hashmap(headers);
        // Parse form data if Content-Type is application/x-www-form-urlencoded
        let form = Self::parse_form_data(headers, body);

        let imposter_port = self.config.port.unwrap_or(0);
        let flow_id = self.resolve_flow_id(&headers_map);
        for (index, stub_state) in stubs.iter().enumerate() {
            let stub = &stub_state.stub;
            // Correlated-isolation gate (issue #223, runs first): a space-scoped stub only
            // participates in matching when the request's resolved flow_id equals its space.
            // Unscoped stubs match any space (PerInstance default).
            if let Some(space) = &stub.space {
                if flow_id != *space {
                    continue;
                }
            }
            // Scenario FSM eligibility gate (before predicate precedence): a stub guarded by
            // `requiredScenarioState` only participates in matching when the current
            // (flow_id, scenario) state equals it.
            if let Some(required) = &stub.required_scenario_state {
                let scenario = stub.scenario_name.as_deref().unwrap_or("");
                if self.scenario_state(&flow_id, scenario) != *required {
                    continue;
                }
            }
            if stub_matches(
                &stub.predicates,
                method,
                path,
                query,
                &headers_map,
                body,
                request_from,
                client_ip,
                form.as_ref(),
                imposter_port,
            ) {
                // TODO(perf): It's unfortunate that we end up deep cloning the whole stub here
                return Some((stub_state.clone(), index));
            }
        }
        None
    }

    /// The configured `flow_id_source` (`"imposter_port"` or `"header:<Name>"`),
    /// defaulting to `"imposter_port"`.
    pub fn flow_id_source(&self) -> String {
        self.config
            .rift
            .as_ref()
            .and_then(|r| r.flow_state.as_ref())
            .and_then(|fs| fs.mountebank_state_mapping.as_ref())
            .map(|m| m.flow_id_source.clone())
            .unwrap_or_else(|| "imposter_port".to_string())
    }

    /// Resolve the correlation `flow_id` for a request, partitioning scenario state.
    /// `"header:<Name>"` uses that (case-insensitive) header; `"imposter_port"` (the default,
    /// and the fallback when the header is absent) uses the imposter port.
    pub fn resolve_flow_id(&self, headers: &HashMap<String, String>) -> String {
        Self::flow_id_for(
            &self.flow_id_source(),
            headers,
            self.config.port.unwrap_or(0),
        )
    }

    /// Pure flow_id resolution (no `&self`), so it can be reused over recorded requests.
    fn flow_id_for(source: &str, headers: &HashMap<String, String>, port: u16) -> String {
        match source.strip_prefix("header:") {
            Some(name) => headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| port.to_string()),
            None => port.to_string(),
        }
    }

    /// Current scenario state for `(flow_id, scenario)`, or the initial state if absent.
    /// A backend read error degrades to the initial state but is logged — it must not be
    /// silently mistaken for "no state yet", which would mis-gate matching.
    pub fn scenario_state(&self, flow_id: &str, scenario: &str) -> String {
        match self.flow_store.get(flow_id, scenario) {
            Ok(Some(v)) => v.as_str().unwrap_or(INITIAL_SCENARIO_STATE).to_string(),
            Ok(None) => INITIAL_SCENARIO_STATE.to_string(),
            Err(e) => {
                warn!(
                    "scenario state read failed ({flow_id}/{scenario}); using initial '{INITIAL_SCENARIO_STATE}': {e}"
                );
                INITIAL_SCENARIO_STATE.to_string()
            }
        }
    }

    /// Set scenario state for `(flow_id, scenario)`.
    pub fn set_scenario_state(
        &self,
        flow_id: &str,
        scenario: &str,
        state: &str,
    ) -> anyhow::Result<()> {
        self.flow_store.set(
            flow_id,
            scenario,
            serde_json::Value::String(state.to_string()),
        )
    }

    /// Delete a scenario's state for a flow (so it reads back as the initial state).
    pub fn delete_scenario_state(&self, flow_id: &str, scenario: &str) -> anyhow::Result<()> {
        self.flow_store.delete(flow_id, scenario)
    }

    /// Apply a matched stub's `newScenarioState` transition after it responds (no-op if unset).
    pub fn apply_scenario_transition(&self, flow_id: &str, stub: &Stub) {
        if let Some(next) = &stub.new_scenario_state {
            let scenario = stub.scenario_name.as_deref().unwrap_or("");
            if let Err(e) = self.set_scenario_state(flow_id, scenario, next) {
                warn!("scenario transition failed ({flow_id}/{scenario} -> {next}): {e}");
            }
        }
    }

    /// Read a raw flow-state value (admin flow-state inspection).
    pub fn flow_get(&self, flow_id: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.flow_store.get(flow_id, key)
    }

    /// Set a raw flow-state value (admin flow-state arrange).
    pub fn flow_set(
        &self,
        flow_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.flow_store.set(flow_id, key, value)
    }

    /// Delete a raw flow-state value (admin flow-state teardown).
    pub fn flow_delete(&self, flow_id: &str, key: &str) -> anyhow::Result<()> {
        self.flow_store.delete(flow_id, key)
    }

    /// Distinct scenario names declared by this imposter's stubs (sorted).
    pub fn scenario_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .stubs
            .read()
            .iter()
            .filter_map(|s| s.stub.scenario_name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Stubs scoped to a given correlation space (issue #223).
    pub fn space_stubs(&self, space: &str) -> Vec<Stub> {
        self.stubs
            .read()
            .iter()
            .filter(|s| s.stub.space.as_deref() == Some(space))
            .map(|s| s.stub.clone())
            .collect()
    }

    /// Tear down a correlation space (issue #223): remove its scoped stubs, drop its recorded
    /// requests, and reset its named scenario states. Other spaces and the port are untouched.
    pub fn teardown_space(&self, space: &str) {
        // Snapshot scenario names BEFORE pruning stubs: a scenario declared only on this space's
        // stubs would otherwise vanish from scenario_names() and its state would never be reset.
        let scenarios = self.scenario_names();
        self.stubs
            .write()
            .retain(|s| s.stub.space.as_deref() != Some(space));
        let source = self.flow_id_source();
        self.recorded_requests.write().retain(|r| {
            Self::flow_id_for(&source, &r.headers, self.config.port.unwrap_or(0)) != space
        });
        for scenario in scenarios {
            if let Err(e) = self.delete_scenario_state(space, &scenario) {
                warn!("space teardown: failed to reset scenario '{scenario}' for '{space}': {e}");
            }
        }
    }

    /// Parse form-urlencoded data from body if Content-Type matches
    fn parse_form_data(
        headers: &hyper::HeaderMap,
        body: Option<&str>,
    ) -> Option<HashMap<String, String>> {
        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if content_type.contains("application/x-www-form-urlencoded") {
            if let Some(body_str) = body {
                let mut map = HashMap::new();
                for pair in body_str.split('&').filter(|s| !s.is_empty()) {
                    let mut parts = pair.splitn(2, '=');
                    if let Some(raw_key) = parts.next() {
                        let key = urlencoding::decode(raw_key)
                            .unwrap_or_default()
                            .into_owned();
                        let value = parts
                            .next()
                            .map(|v| urlencoding::decode(v).unwrap_or_default().into_owned())
                            .unwrap_or_default();
                        map.entry(key)
                            .and_modify(|existing: &mut String| {
                                existing.push(',');
                                existing.push_str(&value);
                            })
                            .or_insert(value);
                    }
                }
                return Some(map);
            }
        }
        None
    }

    /// Get all stubs info for debug purposes (Rift extension)
    pub fn get_all_stubs_info(&self) -> Vec<DebugStubInfo> {
        let stubs = self.stubs.read();
        stubs
            .iter()
            .map(|stub_state| &stub_state.stub)
            .enumerate()
            .map(|(index, stub)| DebugStubInfo {
                index,
                id: stub.id.clone(),
                predicates: stub.predicates.clone(),
                response_count: stub.responses.len(),
            })
            .collect()
    }

    /// Get imposter info for debug purposes (Rift extension)
    pub fn get_debug_imposter_info(&self) -> DebugImposter {
        let stubs = self.stubs.read();
        DebugImposter {
            port: self.config.port.unwrap_or(0),
            name: self.config.name.clone(),
            protocol: self.config.protocol.clone(),
            stub_count: stubs.len(),
        }
    }

    /// Create response preview from a stub (Rift extension)
    pub fn get_response_preview(&self, stub_state: &StubState) -> DebugResponsePreview {
        if stub_state.stub.responses.is_empty() {
            return DebugResponsePreview {
                response_type: "unknown".to_string(),
                status_code: None,
                headers: None,
                body_preview: None,
            };
        }

        // Get the current response from the cycler
        if let Some(response) = stub_state.peek_response() {
            return create_response_preview(response);
        }

        DebugResponsePreview {
            response_type: "unknown".to_string(),
            status_code: None,
            headers: None,
            body_preview: None,
        }
    }

    /// Convert hyper HeaderMap to HashMap<String, String>
    /// Uses Title-Case for header keys to match Mountebank's convention.
    pub(crate) fn header_map_to_hashmap(headers: &hyper::HeaderMap) -> HashMap<String, String> {
        headers
            .iter()
            .map(|(k, v)| {
                (
                    crate::behaviors::header_to_title_case(k.as_str()),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    /// Execute a stub and get the response with behaviors and rift extensions
    /// Returns (status, headers, body, behaviors, rift_extension, response_mode, is_fault)
    #[allow(clippy::type_complexity)]
    pub fn execute_stub_with_rift(
        &self,
        stub_state: &StubState,
    ) -> Option<(
        u16,
        HashMap<String, String>,
        String,
        Option<serde_json::Value>,
        Option<RiftResponseExtension>,
        ResponseMode,
        bool,
    )> {
        let response = stub_state.get_next_response()?;
        execute_stub_response_with_rift(response)
    }

    /// Get RiftScript response if present
    /// Note: This peeks at the current response without advancing the cycler
    pub fn get_rift_script_response(&self, stub_state: &StubState) -> Option<RiftScriptConfig> {
        let response = stub_state.peek_response()?;
        get_rift_script_config(response)
    }

    /// Advance cycler for RiftScript response
    pub fn advance_cycler_for_rift_script(&self, stub_state: &StubState) {
        // Just cycling as a side effect
        _ = stub_state.get_next_response();
    }

    /// Check if a stub response is a proxy and return the proxy config
    /// Note: This peeks at the current response without advancing the cycler
    pub fn get_proxy_response(&self, stub: &StubState) -> Option<ProxyResponse> {
        let response = stub.peek_response()?;

        match response {
            StubResponse::Proxy { proxy } => Some(proxy.clone()),
            _ => None,
        }
    }

    /// Advance the response cycler for a proxy response
    /// This should be called after successfully handling a proxy response
    pub fn advance_cycler_for_proxy(&self, stub_state: &StubState) {
        // Assume proxies won't have a repeat count anyway, so a normal advance works.
        _ = stub_state.get_next_response();
    }

    /// Check if a stub response is an inject and return the inject function
    /// Note: This peeks at the current response without advancing the cycler
    // Used with javascript feature
    pub fn get_inject_response(&self, stub_state: &StubState) -> Option<String> {
        let response = stub_state.peek_response()?;
        match response {
            StubResponse::Inject { inject } => Some(inject.clone()),
            _ => None,
        }
    }

    /// Advance the response cycler for an inject response
    /// This should be called after successfully handling an inject response
    // Used with javascript feature
    pub fn advance_cycler_for_inject(&self, stub_state: &StubState) {
        _ = stub_state.get_next_response();
    }

    /// Generate predicates from request based on predicateGenerators config
    fn generate_predicates_from_request(
        &self,
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        body: Option<&str>,
        query: Option<&str>,
    ) -> Vec<serde_json::Value> {
        let mut predicates = Vec::new();

        for gen in generators {
            let gen_obj = match gen.as_object() {
                Some(obj) => obj,
                None => continue,
            };

            // Handle inject predicateGenerator — calls a JS function with the request and
            // predicates built so far; the function returns additional predicate objects.
            if let Some(inject_fn) = gen_obj.get("inject").and_then(|v| v.as_str()) {
                #[cfg(feature = "javascript")]
                {
                    use crate::scripting::{execute_predicate_generator_inject, MountebankRequest};
                    let query_map = query
                        .map(crate::imposter::parse_query_string)
                        .unwrap_or_default();
                    let mb_request = MountebankRequest {
                        method: method.to_string(),
                        path: path.to_string(),
                        query: query_map,
                        headers: headers.clone(),
                        body: body.map(|b| b.to_string()),
                    };
                    let inject_preds =
                        execute_predicate_generator_inject(inject_fn, &mb_request, &predicates);
                    predicates.extend(inject_preds);
                }
                #[cfg(not(feature = "javascript"))]
                {
                    tracing::warn!("predicateGenerator inject requires the 'javascript' feature; generator ignored");
                    let _ = inject_fn;
                }
                continue;
            }

            // Get the matches config
            let matches = match gen_obj.get("matches").and_then(|m| m.as_object()) {
                Some(m) => m,
                None => continue,
            };

            // Get options
            let case_sensitive = gen_obj
                .get("caseSensitive")
                .and_then(|c| c.as_bool())
                .unwrap_or(true);
            let predicate_operator = gen_obj
                .get("predicateOperator")
                .and_then(|p| p.as_str())
                .unwrap_or("equals");
            let except_pattern = gen_obj.get("except").and_then(|e| e.as_str());

            // Build predicate values
            let mut pred_values = serde_json::Map::new();

            // Handle path
            if matches
                .get("path")
                .and_then(|p| p.as_bool())
                .unwrap_or(false)
            {
                let mut path_val = path.to_string();
                // Apply except pattern if present
                if let Some(pattern) = except_pattern {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        path_val = re.replace_all(&path_val, "").to_string();
                    }
                }
                pred_values.insert("path".to_string(), serde_json::Value::String(path_val));
            }

            // Handle method
            if matches
                .get("method")
                .and_then(|m| m.as_bool())
                .unwrap_or(false)
            {
                let mut method_val = method.to_string();
                if let Some(pattern) = except_pattern {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        method_val = re.replace_all(&method_val, "").to_string();
                    }
                }
                pred_values.insert("method".to_string(), serde_json::Value::String(method_val));
            }

            // Handle query
            if matches
                .get("query")
                .and_then(|q| q.as_bool())
                .unwrap_or(false)
            {
                if let Some(query_str) = query {
                    let query_map = crate::imposter::parse_query_string(query_str);
                    if !query_map.is_empty() {
                        let query_json: serde_json::Map<String, serde_json::Value> = query_map
                            .into_iter()
                            .map(|(k, v)| (k, serde_json::Value::String(v)))
                            .collect();
                        pred_values
                            .insert("query".to_string(), serde_json::Value::Object(query_json));
                    }
                }
            }

            // Handle headers
            if let Some(header_matches) = matches.get("headers").and_then(|h| h.as_object()) {
                let mut header_preds = serde_json::Map::new();
                for (header_name, should_match) in header_matches {
                    if should_match.as_bool().unwrap_or(false) {
                        if let Some(header_value) = headers.get(header_name) {
                            header_preds.insert(
                                header_name.clone(),
                                serde_json::Value::String(header_value.clone()),
                            );
                        }
                    }
                }
                if !header_preds.is_empty() {
                    pred_values.insert(
                        "headers".to_string(),
                        serde_json::Value::Object(header_preds),
                    );
                }
            }

            // Handle body
            if matches
                .get("body")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
            {
                if let Some(body_str) = body {
                    let mut body_val = body_str.to_string();
                    // Apply except pattern if present
                    if let Some(pattern) = except_pattern {
                        if let Ok(re) = regex::Regex::new(pattern) {
                            body_val = re.replace_all(&body_val, "").to_string();
                        }
                    }
                    pred_values.insert("body".to_string(), serde_json::Value::String(body_val));
                }
            }

            if pred_values.is_empty() {
                continue;
            }

            // Build the predicate with the operator
            let mut predicate = serde_json::Map::new();
            predicate.insert(
                predicate_operator.to_string(),
                serde_json::Value::Object(pred_values),
            );

            // Always write caseSensitive so the matcher sees the generator's intent
            predicate.insert(
                "caseSensitive".to_string(),
                serde_json::Value::Bool(case_sensitive),
            );

            predicates.push(serde_json::Value::Object(predicate));
        }

        predicates
    }

    /// Insert a generated stub at the specified index
    pub fn insert_generated_stub(&self, stub: Stub, before_index: usize) {
        let new_stub_state = StubState::new(stub);
        let mut stubs = self.stubs.write();
        let index = before_index.min(stubs.len());
        stubs.insert(index, new_stub_state);
        debug!("Inserted generated stub at index {}", index);
    }

    /// Insert or append a generated stub based on proxy mode.
    ///
    /// Instead of trusting a previously-obtained stub index (which may be stale
    /// if concurrent requests modified the stub list), this method re-locates the
    /// proxy stub under the write lock using `proxy_to` as identifier.
    ///
    /// For proxyOnce: Insert new stub BEFORE the proxy stub (so it matches first next time)
    /// For proxyAlways: Append response to existing stub AFTER proxy stub, or insert new AFTER proxy
    pub fn insert_or_append_proxy_stub(&self, stub: Stub, proxy_to: &str, proxy_mode: &str) {
        let mut stubs = self.stubs.write();

        // Re-locate the proxy stub under the write lock to avoid stale-index races.
        let proxy_stub_index = stubs
            .iter()
            .position(|s| {
                s.stub
                    .responses
                    .iter()
                    .any(|r| matches!(r, StubResponse::Proxy { proxy } if proxy.to == proxy_to))
            })
            .unwrap_or(stubs.len());

        if proxy_mode == "proxyAlways" {
            // For proxyAlways, recorded stubs go AFTER the proxy stub
            // This ensures proxy always runs first and records each request

            // Try to find existing stub with matching predicates (after the proxy stub)
            let matching_stub_idx = stubs
                .iter()
                .map(|stub_state| &stub_state.stub)
                .enumerate()
                .skip(proxy_stub_index + 1) // Only look after the proxy stub
                .find(|(_, existing)| {
                    // Compare predicates (JSON comparison)
                    let existing_preds =
                        serde_json::to_string(&existing.predicates).unwrap_or_default();
                    let new_preds = serde_json::to_string(&stub.predicates).unwrap_or_default();
                    existing_preds == new_preds && !existing.predicates.is_empty()
                })
                .map(|(idx, _)| idx);

            if let Some(idx) = matching_stub_idx {
                // Append responses to existing stub
                stubs[idx].stub.responses.extend(stub.responses);
                debug!(
                    "Appended response to existing stub at index {} (proxyAlways mode, {} total responses)",
                    idx,
                    stubs[idx].stub.responses.len()
                );
                return;
            }

            // No matching stub found: insert new stub AFTER the proxy stub
            let insert_index = (proxy_stub_index + 1).min(stubs.len());
            stubs.insert(insert_index, StubState::new(stub));
            debug!(
                "Inserted generated stub at index {} after proxy (proxyAlways mode)",
                insert_index
            );
        } else {
            // For proxyOnce: insert new stub BEFORE the proxy stub
            // This ensures the recorded stub matches first on subsequent requests
            let index = proxy_stub_index.min(stubs.len());
            stubs.insert(index, StubState::new(stub));
            debug!(
                "Inserted generated stub at index {} before proxy (proxyOnce mode)",
                index
            );
        }
    }

    /// Forward a request through proxy and optionally record the response
    pub async fn handle_proxy_request(
        &self,
        proxy_config: &ProxyResponse,
        method: &str,
        uri: &hyper::Uri,
        headers: &HashMap<String, String>,
        body: Option<&str>,
    ) -> anyhow::Result<(u16, Vec<(String, String)>, Vec<u8>, Option<u64>)> {
        let client = get_http_client();

        info!("Proxy config - addDecorateBehavior: {:?}, addWaitBehavior: {}, predicateGenerators: {:?}",
            proxy_config.add_decorate_behavior, proxy_config.add_wait_behavior, proxy_config.predicate_generators);

        // Build the proxy URL, applying path rewrite if configured
        let original_path = uri.path();
        let rewritten_path = if let Some(ref rewrite) = proxy_config.path_rewrite {
            original_path.replacen(&rewrite.from, &rewrite.to, 1)
        } else {
            original_path.to_string()
        };

        let target_url = format!(
            "{}{}{}",
            proxy_config.to,
            rewritten_path,
            uri.query().map(|q| format!("?{q}")).unwrap_or_default()
        );

        if proxy_config.path_rewrite.is_some() {
            debug!(
                "Proxy request to: {} (path rewritten from '{}')",
                target_url, original_path
            );
        } else {
            debug!("Proxy request to: {}", target_url);
        }

        // Create request signature for recording
        let signature = RequestSignature::new(method, uri.path(), uri.query(), &[]);

        // Check if we should replay cached response (based on proxy mode)
        if !self.recording_store.should_proxy(&signature) {
            if let Some(recorded) = self.recording_store.get_recorded(&signature) {
                debug!("Returning recorded proxy response (proxyOnce mode)");
                return Ok((
                    recorded.status,
                    recorded.headers.clone(),
                    recorded.body.clone(),
                    recorded.latency_ms,
                ));
            }
        }

        // Forward the request
        let start = Instant::now();

        let mut request = match method.to_uppercase().as_str() {
            "GET" => client.get(&target_url),
            "POST" => client.post(&target_url),
            "PUT" => client.put(&target_url),
            "DELETE" => client.delete(&target_url),
            "PATCH" => client.patch(&target_url),
            "HEAD" => client.head(&target_url),
            _ => client.get(&target_url),
        };

        // Copy headers (excluding host)
        for (key, value) in headers {
            let key_lower = key.to_lowercase();
            if key_lower != "host" && key_lower != "content-length" {
                request = request.header(key, value);
            }
        }

        // Add inject headers
        for (key, value) in &proxy_config.inject_headers {
            request = request.header(key, value);
        }

        // Add body if present
        if let Some(body_str) = body {
            request = request.body(body_str.to_string());
        }

        // Send request
        let response = request
            .send()
            .await
            .with_context(|| format!("Failed to send proxy request to {}", target_url))?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let status = response.status().as_u16();
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        // Check Content-Length before reading the full body to reject obviously oversized responses
        if let Some(content_length) = response.content_length() {
            if content_length as usize > MAX_PROXY_RESPONSE_BODY_SIZE {
                anyhow::bail!(
                    "Proxy response body from {} exceeds maximum size ({} > {} bytes)",
                    target_url,
                    content_length,
                    MAX_PROXY_RESPONSE_BODY_SIZE
                );
            }
        }

        let body_bytes = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read response body from {}", target_url))?;

        if body_bytes.len() > MAX_PROXY_RESPONSE_BODY_SIZE {
            anyhow::bail!(
                "Proxy response body from {} exceeds maximum size ({} > {} bytes)",
                target_url,
                body_bytes.len(),
                MAX_PROXY_RESPONSE_BODY_SIZE
            );
        }

        // Record the response
        let recorded_response = RecordedResponse {
            status,
            headers: response_headers.clone(),
            body: body_bytes.to_vec(),
            latency_ms: if proxy_config.add_wait_behavior {
                Some(latency_ms)
            } else {
                None
            },
            timestamp_secs: crate::util::unix_timestamp(),
        };

        self.recording_store.record(signature, recorded_response);

        // Generate and insert stub if predicateGenerators, addWaitBehavior, or addDecorateBehavior is configured
        // (Mountebank generates stubs automatically when these are enabled)
        if !proxy_config.predicate_generators.is_empty()
            || proxy_config.add_wait_behavior
            || proxy_config.add_decorate_behavior.is_some()
        {
            let predicates = if !proxy_config.predicate_generators.is_empty() {
                self.generate_predicates_from_request(
                    &proxy_config.predicate_generators,
                    method,
                    uri.path(),
                    headers,
                    body,
                    uri.query(),
                )
            } else {
                // No predicateGenerators, generate empty predicates (matches all requests)
                vec![]
            };

            let latency_for_stub = if proxy_config.add_wait_behavior {
                Some(latency_ms)
            } else {
                None
            };

            // Note: addDecorateBehavior is added to the SAVED stub's behaviors,
            // not applied to the first (live proxy) response. This matches Mountebank's behavior.
            // The decoration will be applied when the saved stub is used for subsequent requests.

            let new_stub = create_stub_from_proxy_response(
                predicates,
                status,
                &response_headers,
                &body_bytes,
                latency_for_stub,
                proxy_config.add_decorate_behavior.clone(),
                Some(proxy_config.to.clone()),
            );

            // Insert or append the stub based on proxy mode
            // proxyOnce: Insert new stub before the proxy stub
            // proxyAlways: Append response to existing stub with matching predicates
            let mode = if proxy_config.mode.is_empty() {
                "proxyOnce"
            } else {
                &proxy_config.mode
            };
            self.insert_or_append_proxy_stub(new_stub, &proxy_config.to, mode);
            debug!(
                "Generated stub from proxy response for path {} (mode: {})",
                uri.path(),
                mode
            );
        }

        Ok((
            status,
            response_headers,
            body_bytes.to_vec(),
            if proxy_config.add_wait_behavior {
                Some(latency_ms)
            } else {
                None
            },
        ))
    }

    /// Record a request. Evicts the oldest entry when the cap is reached.
    pub fn record_request(&self, req: &RecordedRequest) {
        if self.config.record_requests {
            let mut requests = self.recorded_requests.write();
            if requests.len() >= MAX_RECORDED_REQUESTS {
                tracing::warn!(
                    port = self.config.port,
                    max = MAX_RECORDED_REQUESTS,
                    "Recorded requests cap reached; oldest entry evicted"
                );
                requests.remove(0);
            }
            requests.push(req.clone());
        }
    }

    /// Get recorded requests
    pub fn get_recorded_requests(&self) -> Vec<RecordedRequest> {
        self.recorded_requests.read().clone()
    }

    /// Clear recorded requests
    pub fn clear_recorded_requests(&self) {
        self.recorded_requests.write().clear();
        // Reset request count to match Mountebank behavior
        self.request_count.store(0, Ordering::SeqCst);
    }

    /// Retain only the recorded requests for which `keep` returns true.
    /// Used for targeted clears (a single correlated slice); unlike
    /// `clear_recorded_requests` it does not reset the total request count,
    /// since other slices' requests remain.
    pub fn retain_recorded_requests<F: Fn(&RecordedRequest) -> bool>(&self, keep: F) {
        self.recorded_requests.write().retain(|r| keep(r));
    }

    /// Clear saved proxy responses
    pub fn clear_proxy_responses(&self) {
        self.recording_store.clear();
    }

    /// Increment request count
    pub fn increment_request_count(&self) -> u64 {
        self.request_count.fetch_add(1, Ordering::SeqCst)
    }

    /// Get request count
    pub fn get_request_count(&self) -> u64 {
        self.request_count.load(Ordering::SeqCst)
    }

    /// Add a stub at a specific index
    pub fn add_stub(&self, stub: Stub, index: Option<usize>) {
        let mut stubs = self.stubs.write();
        let idx = index.unwrap_or(stubs.len());
        let idx = idx.min(stubs.len());
        stubs.insert(idx, StubState::new(stub));
    }

    /// Replace a stub at a specific index
    pub fn replace_stub(&self, index: usize, stub: Stub) -> Result<(), String> {
        let mut stubs = self.stubs.write();
        if index >= stubs.len() {
            return Err(format!("Stub index {index} out of bounds"));
        }
        stubs[index].stub = stub;
        Ok(())
    }

    /// Delete a stub at a specific index
    pub fn delete_stub(&self, index: usize) -> Result<(), String> {
        let mut stubs = self.stubs.write();
        if index >= stubs.len() {
            return Err(format!("Stub index {index} out of bounds"));
        }
        stubs.remove(index);
        Ok(())
    }

    /// Get all stubs
    pub fn get_stubs(&self) -> Vec<Stub> {
        self.stubs
            .read()
            .iter()
            .map(|stub_state| stub_state.stub.clone())
            .collect()
    }

    /// Get a specific stub by index
    pub fn get_stub(&self, index: usize) -> Option<Stub> {
        let stubs = self.stubs.read();
        stubs.get(index).map(|stub_state| stub_state.stub.clone())
    }

    /// Set enabled state
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Check if enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::types::ImposterConfig;
    use serde_json::json;

    fn make_test_imposter() -> Imposter {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            ..Default::default()
        };
        Imposter::new(config)
    }

    // Fix #95: Multi-valued form fields are now comma-joined instead of overwritten
    #[test]
    fn test_parse_form_data_multi_valued_fields() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().unwrap(),
        );

        let result = Imposter::parse_form_data(&headers, Some("checkbox=A&checkbox=B&checkbox=C"));
        let form = result.expect("Should parse form data");
        assert_eq!(
            form.get("checkbox").unwrap(),
            "A,B,C",
            "Multi-valued form fields should be comma-joined"
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
        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "GET",
            "/API/Users",
            &headers,
            None,
            None,
        );

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
        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "POST",
            "/test",
            &headers,
            None,
            None,
        );

        assert_eq!(predicates.len(), 1);
        let pred_json = &predicates[0];
        let method_val = pred_json["equals"]["method"].as_str().unwrap();

        assert_eq!(
            method_val, "",
            "except pattern should be applied to method in predicate generator"
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
        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "GET",
            "/search",
            &headers,
            None,
            Some("q=hello&page=1"),
        );

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

        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "GET",
            "/api/users",
            &headers,
            None,
            None,
        );

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

        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "POST",
            "/orders",
            &headers,
            None,
            None,
        );

        // matches generator produces 1, inject generator returns 2 (original + new path)
        assert_eq!(predicates.len(), 3);
    }

    #[test]
    fn test_record_request_cap_enforced() {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            record_requests: true,
            ..Default::default()
        };
        let imposter = Imposter::new(config);
        let req = RecordedRequest {
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };

        for _ in 0..MAX_RECORDED_REQUESTS + 10 {
            imposter.record_request(&req);
        }

        let recorded = imposter.recorded_requests.read();
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
        let imposter = Imposter::new(config);

        let req = |space: &str| {
            let mut headers = std::collections::HashMap::new();
            headers.insert("X-Mock-Space".to_string(), space.to_string());
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

        imposter.retain_recorded_requests(|r| r.headers.get("X-Mock-Space").unwrap() != "A");
        assert_eq!(imposter.get_recorded_requests().len(), 1, "only B kept");
        assert_eq!(
            imposter.get_request_count(),
            3,
            "targeted retain must not reset the request count"
        );

        imposter.clear_recorded_requests();
        assert_eq!(imposter.get_recorded_requests().len(), 0);
        assert_eq!(
            imposter.get_request_count(),
            0,
            "full clear resets the request count"
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_bad_function_returns_empty() {
        let imposter = make_test_imposter();

        let generators = vec![json!({ "inject": "not a function" })];
        let headers = HashMap::new();

        let predicates = imposter.generate_predicates_from_request(
            &generators,
            "GET",
            "/test",
            &headers,
            None,
            None,
        );

        assert!(predicates.is_empty());
    }
}
