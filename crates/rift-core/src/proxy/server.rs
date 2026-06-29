//! ProxyServer struct and main run loop.
//!
//! This module contains the ProxyServer struct which holds all state,
//! and the main run loop that accepts connections and handles requests.

use super::client::{create_http_client, should_skip_tls_verify, HttpClient};
use super::handler::handle_request;
use super::network::create_reusable_listener;
use super::tls::create_tls_acceptor;
use crate::behaviors::{CsvCache, ResponseCycler};
use crate::config::{Config, Protocol as RiftProtocol, Upstream};
use crate::extensions::flow_state::{create_flow_store, FlowStore};
use crate::extensions::matcher::CompiledRule;
use crate::extensions::routing::Router;
use crate::proxy::context::RequestHandlerContext;
use crate::recording::{ProxyMode, RecordingStore};
#[cfg(feature = "javascript")]
use crate::scripting::compile_js_to_bytecode;
#[cfg(feature = "lua")]
use crate::scripting::compile_to_bytecode;
use crate::scripting::RhaiEngine;
use crate::scripting::{
    CompiledScript, DecisionCache, DecisionCacheConfig, ScriptPool, ScriptPoolConfig,
};

#[cfg(any(feature = "lua", feature = "javascript"))]
use anyhow::Context;
use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};

/// The main proxy server struct.
pub struct ProxyServer {
    config: Arc<Config>,
    compiled_rules: Arc<Vec<CompiledRule>>,
    rule_upstreams: Arc<Vec<Option<String>>>, // Upstream filter for each rule (parallel to compiled_rules)
    upstream_uri: String,                     // Used for sidecar mode
    upstreams: Vec<Upstream>,                 // Used for reverse proxy mode
    router: Option<Router>,
    flow_store: Arc<dyn FlowStore>, // Flow store for scripts (may be NoOp if not configured)
    script_pool: Option<Arc<ScriptPool>>, // Script pool for optimized execution
    compiled_scripts: Option<Vec<(CompiledScript, CompiledRule, Option<String>)>>, // Precompiled scripts for pool
    decision_cache: Option<Arc<DecisionCache>>, // Decision cache for memoization
    http_client: HttpClient,                    // Shared HTTP client for HTTP/1.1
    // Mountebank-compatible behavior state
    // Will be wired up when response cycling is fully integrated
    response_cycler: Arc<ResponseCycler>, // Response cycling state (repeat behavior)
    csv_cache: Arc<CsvCache>,             // CSV data cache (lookup behavior)
    recording_store: Arc<RecordingStore>, // Recording store (proxyOnce/proxyAlways modes)
}

impl ProxyServer {
    /// Create a new ProxyServer from configuration.
    pub async fn new(config: Config) -> Result<Self, anyhow::Error> {
        Self::new_internal(config, None).await
    }

    /// Create a new ProxyServer with a shared flow store.
    pub async fn new_with_shared_flow_store(
        config: Config,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<Self, anyhow::Error> {
        Self::new_internal(config, Some(flow_store)).await
    }

    async fn new_internal(
        config: Config,
        shared_flow_store: Option<Arc<dyn FlowStore>>,
    ) -> Result<Self, anyhow::Error> {
        // Compile rules and extract upstream filters
        let mut compiled_rules = Vec::new();
        let mut rule_upstreams = Vec::new();

        for rule in &config.rules {
            compiled_rules.push(CompiledRule::compile(rule.clone())?);
            rule_upstreams.push(rule.upstream.clone());
        }

        // Get upstream URI (backward compatible with sidecar mode)
        let upstream_uri = if let Some(ref upstream) = config.upstream {
            let protocol = upstream.get_protocol();
            format!(
                "{}://{}:{}",
                protocol.as_str(),
                upstream.host,
                upstream.port
            )
        } else if !config.upstreams.is_empty() {
            // For reverse proxy mode, use first upstream as fallback
            config.upstreams[0].url.clone()
        } else {
            anyhow::bail!("Config must specify either 'upstream' (sidecar mode) or 'upstreams' (reverse proxy mode)");
        };

        // Create router for multi-upstream mode
        let router = if !config.routing.is_empty() {
            let r = Router::new(config.routing.clone())
                .map_err(|e| anyhow::anyhow!("Failed to create router: {e}"))?;
            Some(r)
        } else {
            None
        };

        // Use shared flow store if provided, otherwise initialize new one
        let flow_store: Arc<dyn FlowStore> = if let Some(store) = shared_flow_store {
            // Using shared flow store across workers
            store
        } else if let Some(ref fs_config) = config.flow_state {
            // Create new flow store for this worker (backward compatible mode)
            create_flow_store(fs_config)?
        } else if !config.script_rules.is_empty() {
            // Scripts are configured but no flow_state - use no-op store
            tracing::info!("Using NoOpFlowStore for scripts (flow_state not configured)");
            Arc::new(crate::extensions::flow_state::NoOpFlowStore)
        } else {
            // Neither scripts nor flow_state configured - use no-op store as placeholder
            Arc::new(crate::extensions::flow_state::NoOpFlowStore)
        };

        // Create script pool and decision cache for script execution
        let (script_pool, compiled_scripts, decision_cache) = if !config.script_rules.is_empty() {
            let mut scripts = Vec::new();
            let engine_type = config
                .script_engine
                .as_ref()
                .map(|cfg| cfg.engine.as_str())
                .unwrap_or("rhai");

            for script_rule in &config.script_rules {
                // Compile script to appropriate format
                let compiled = match engine_type {
                    "rhai" => {
                        let engine = RhaiEngine::new(&script_rule.script, script_rule.id.clone())?;
                        CompiledScript::Rhai {
                            ast: engine.ast().clone(),
                            rule_id: script_rule.id.clone(),
                        }
                    }
                    #[cfg(feature = "lua")]
                    "lua" => {
                        let bytecode =
                            compile_to_bytecode(&script_rule.script).with_context(|| {
                                format!(
                                    "Failed to compile Lua script for rule '{}'",
                                    script_rule.id
                                )
                            })?;
                        CompiledScript::Lua {
                            bytecode: Arc::new(bytecode),
                            rule_id: script_rule.id.clone(),
                        }
                    }
                    #[cfg(not(feature = "lua"))]
                    "lua" => {
                        anyhow::bail!("Lua engine not enabled. Enable the 'lua' feature flag")
                    }
                    #[cfg(feature = "javascript")]
                    "javascript" | "js" => {
                        let bytecode =
                            compile_js_to_bytecode(&script_rule.script).with_context(|| {
                                format!(
                                    "Failed to compile JavaScript script for rule '{}'",
                                    script_rule.id
                                )
                            })?;
                        CompiledScript::JavaScript {
                            bytecode: Arc::new(bytecode),
                            rule_id: script_rule.id.clone(),
                        }
                    }
                    #[cfg(not(feature = "javascript"))]
                    "javascript" | "js" => {
                        anyhow::bail!(
                            "JavaScript engine not enabled. Enable the 'javascript' feature flag"
                        )
                    }
                    other => anyhow::bail!("Unknown script engine type: {other}"),
                };

                let matcher = CompiledRule::compile(crate::config::Rule {
                    id: script_rule.id.clone(),
                    match_config: script_rule.match_config.clone(),
                    fault: Default::default(),
                    upstream: None,
                })?;

                scripts.push((compiled, matcher, script_rule.upstream.clone()));
            }

            // Create script pool with config (or defaults)
            let pool_config = if let Some(ref pool_cfg) = config.script_pool {
                ScriptPoolConfig {
                    workers: pool_cfg.workers,
                    queue_size: pool_cfg.queue_size,
                    timeout_ms: pool_cfg.timeout_ms,
                }
            } else {
                ScriptPoolConfig::default()
            };
            let pool = Arc::new(ScriptPool::new(pool_config.clone())?);
            info!(
                "Script pool initialized with {} workers",
                pool_config.workers
            );

            // Create decision cache with config (or defaults)
            let cache_config = if let Some(ref cache_cfg) = config.decision_cache {
                DecisionCacheConfig {
                    enabled: cache_cfg.enabled,
                    max_size: cache_cfg.max_size,
                    ttl_seconds: cache_cfg.ttl_seconds,
                }
            } else {
                DecisionCacheConfig::default()
            };
            let cache = Arc::new(DecisionCache::new(cache_config.clone()));
            info!(
                "Decision cache initialized: enabled={}, max_size={}, ttl={}s",
                cache_config.enabled, cache_config.max_size, cache_config.ttl_seconds
            );

            (Some(pool), Some(scripts), Some(cache))
        } else {
            (None, None, None)
        };

        let upstreams = config.upstreams.clone();

        // Check if any upstream needs TLS verification skipped
        let skip_tls_verify = should_skip_tls_verify(&config);

        // Create shared HTTP client
        let http_client = create_http_client(&config, skip_tls_verify);

        // Extract recording mode before moving config into Arc
        let recording_mode = config.recording.mode;

        Ok(Self {
            config: Arc::new(config),
            compiled_rules: Arc::new(compiled_rules),
            rule_upstreams: Arc::new(rule_upstreams),
            upstream_uri,
            upstreams,
            router,
            flow_store,
            script_pool,
            compiled_scripts,
            decision_cache,
            http_client,
            // Initialize behavior state
            response_cycler: Arc::new(ResponseCycler::new()),
            csv_cache: Arc::new(CsvCache::new()),
            recording_store: Arc::new(RecordingStore::new(recording_mode)),
        })
    }

    /// Run the proxy server, accepting connections and handling requests.
    pub async fn run(self) -> Result<(), anyhow::Error> {
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.listen.port));
        let listener = create_reusable_listener(addr)?;
        let protocol = self.config.listen.protocol;

        // Create TLS acceptor if protocol is HTTPS
        let tls_acceptor = if protocol == RiftProtocol::Https {
            let tls_config =
                self.config.listen.tls.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("TLS configuration required for HTTPS listener")
                })?;
            Some(create_tls_acceptor(
                &tls_config.cert_path,
                &tls_config.key_path,
            )?)
        } else {
            None
        };

        info!("Listening on {}://{}", protocol.as_str(), addr);
        info!("Proxying to {}", self.upstream_uri);
        info!("Loaded {} fault injection rules", self.compiled_rules.len());
        if let Some(ref scripts) = self.compiled_scripts {
            info!("Loaded {} script rules", scripts.len());
        }
        if self.recording_store.mode() != ProxyMode::ProxyTransparent {
            info!("Recording mode: {:?}", self.recording_store.mode());
        }

        let server = Arc::new(self);

        loop {
            let (stream, remote_addr) = listener.accept().await?;
            let server = Arc::clone(&server);
            let tls_acceptor = tls_acceptor.clone();

            tokio::spawn(async move {
                match protocol {
                    RiftProtocol::Https => {
                        // HTTPS: perform TLS handshake first
                        let Some(acceptor) = tls_acceptor else {
                            error!(
                                "TLS acceptor missing for HTTPS connection from {}",
                                remote_addr
                            );
                            return;
                        };
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = TokioIo::new(tls_stream);
                                let service = service_fn(move |req| {
                                    let server = Arc::clone(&server);
                                    async move { server.handle_request_internal(req).await }
                                });

                                if let Err(err) =
                                    http1::Builder::new().serve_connection(io, service).await
                                {
                                    error!(
                                        "Error serving HTTPS connection from {}: {}",
                                        remote_addr, err
                                    );
                                }
                            }
                            Err(err) => {
                                error!("TLS handshake failed from {}: {}", remote_addr, err);
                            }
                        }
                    }
                    RiftProtocol::Http => {
                        // HTTP: serve directly
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            let server = Arc::clone(&server);
                            async move { server.handle_request_internal(req).await }
                        });

                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await
                        {
                            error!(
                                "Error serving HTTP connection from {}: {}",
                                remote_addr, err
                            );
                        }
                    }
                    _ => {
                        error!("Unsupported protocol: {}", protocol.as_str());
                    }
                }
            });
        }
    }

    /// Internal request handler that builds the context and delegates to handler module.
    async fn handle_request_internal(
        &self,
        req: hyper::Request<hyper::body::Incoming>,
    ) -> Result<hyper::Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        // Build recording signature headers from config
        let signature_headers: Vec<(String, String)> = self
            .config
            .recording
            .predicate_generators
            .iter()
            .flat_map(|pg| pg.matches.headers.iter())
            .filter_map(|header_name| {
                req.headers()
                    .get(header_name)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| (header_name.clone(), v.to_string()))
            })
            .collect();

        let ctx = RequestHandlerContext {
            http_client: &self.http_client,
            compiled_rules: &self.compiled_rules,
            rule_upstreams: &self.rule_upstreams,
            upstream_uri: &self.upstream_uri,
            router: self.router.as_ref(),
            upstreams: &self.upstreams,
            flow_store: &self.flow_store,
            script_pool: self.script_pool.as_ref(),
            compiled_scripts: self.compiled_scripts.as_deref(),
            decision_cache: self.decision_cache.as_ref(),
            csv_cache: &self.csv_cache,
            recording_store: &self.recording_store,
            recording_signature_headers: &signature_headers,
            flow_state_configured: self.config.flow_state.is_some(),
        };

        handle_request(&ctx, req).await
    }
}
