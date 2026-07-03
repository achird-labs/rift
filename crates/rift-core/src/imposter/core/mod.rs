//! Core Imposter struct and implementation.
//!
//! This module contains the Imposter struct which represents a single
//! running imposter instance with its configuration, stubs, and state.

/// Default scenario state when a `(flow_id, scenario)` entry is absent (WireMock parity).
pub const INITIAL_SCENARIO_STATE: &str = "Started";

use super::predicates::stub_matches;
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
use parking_lot::RwLock;
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
            .pool_max_idle_per_host(0) // Disable connection pooling to avoid stale connections
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
}

/// Runtime state of an imposter
pub struct Imposter {
    pub config: ImposterConfig,
    /// Mutable stubs (can be modified at runtime)
    pub stubs: RwLock<Vec<StubState>>,
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
}

impl Imposter {
    /// Create a new imposter from config (no custom flow-store provider).
    pub fn new(config: ImposterConfig) -> Self {
        Self::new_with_provider(config, None)
    }

    /// Create a new imposter, consulting `provider` for its flow store before the built-in
    /// `_rift.flowState` selection (issue #312).
    pub fn new_with_provider(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
    ) -> Self {
        Self::new_with_hooks(config, provider, None)
    }

    /// Create a new imposter with all embedder hooks: the flow-store `provider` (#312) and
    /// a pluggable response `sequencer` (#313; None = embedded per-stub cycler).
    pub fn new_with_hooks(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
    ) -> Self {
        Self::new_with_hooks_and_journal(config, provider, sequencer, None)
    }

    /// Create a new imposter with all embedder hooks, including the request journal (#314).
    ///
    /// `config.port` must be resolved (Some) before the imposter serves requests: a shared
    /// journal keys by port, and unresolved ports would silently multiplex onto slot 0.
    pub fn new_with_hooks_and_journal(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
        journal: Option<Arc<dyn crate::imposter::journal::RequestJournal>>,
    ) -> Self {
        let stubs: Vec<StubState> = config
            .stubs
            .iter()
            .map(|stub| StubState::new(stub.clone()))
            .collect();

        // Extract proxy mode from stubs (use first proxy response's mode)
        let proxy_mode = Self::extract_proxy_mode(&config.stubs);

        // Initialize flow store: a registered provider wins; otherwise the built-in
        // `_rift.flowState` selection.
        let flow_store = Self::create_flow_store(&config, provider);

        Self {
            config,
            stubs: RwLock::new(stubs),
            proxy_store: Arc::new(LocalProxyStore::new(proxy_mode)),
            journal: journal
                .unwrap_or_else(|| Arc::new(crate::imposter::journal::LocalJournal::default())),
            enabled: AtomicBool::new(true),
            created_at: chrono::Utc::now(),
            shutdown_tx: None,
            flow_store,
            sequencer,
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
    /// creation, configure `_rift.flowState`, use `PUT /imposters` (which recreates), or set a
    /// manager-scoped `FlowStoreProvider` returning a shared store (issue #312).
    fn create_flow_store(
        config: &ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
    ) -> Arc<dyn FlowStore> {
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
            return store;
        }
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
                #[cfg(feature = "test-backend")]
                "failing" => {
                    info!("Creating deliberately failing FlowStore (test-backend feature)");
                    Arc::new(crate::extensions::flow_state::FailingFlowStore)
                }
                other => {
                    warn!("Unknown flow state backend '{}', using NoOp", other);
                    Arc::new(NoOpFlowStore)
                }
            };
        }

        if Self::uses_scenario_fsm(&config.stubs) {
            info!("Stubs declare scenario state; using default in-memory FlowStore");
            return Arc::new(InMemoryFlowStore::new(DEFAULT_FLOW_STATE_TTL_SECS));
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
            error!(
                "Redis backend not available (compile with --features redis-backend), falling back to NoOp"
            );
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
}

mod lifecycle;
mod matching;
mod proxy;
mod recording;
mod responses;

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
        let imp = Imposter::new(cfg);

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
        let imposter = Imposter::new(config);

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
