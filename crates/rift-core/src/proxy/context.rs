use crate::behaviors::CsvCache;
use crate::extensions::{CompiledRule, FlowStore, Router};
use crate::proxy::client::HttpClient;
use crate::recording::RecordingStore;
use crate::scripting::{CompiledScript, DecisionCache, ScriptPool};
use bytes::Bytes;
use hyper::Request;
use std::sync::Arc;

/// Context for handling a request, containing all necessary state.
pub struct RequestHandlerContext<'a> {
    pub http_client: &'a HttpClient,
    pub compiled_rules: &'a [CompiledRule],
    pub rule_upstreams: &'a [Option<String>],
    pub upstream_uri: &'a str,
    pub router: Option<&'a Router>,
    pub upstreams: &'a [crate::config::Upstream],
    pub flow_store: &'a Arc<dyn FlowStore>,
    pub script_pool: Option<&'a Arc<ScriptPool>>,
    pub compiled_scripts: Option<&'a [(CompiledScript, CompiledRule, Option<String>)]>,
    pub decision_cache: Option<&'a Arc<DecisionCache>>,
    pub csv_cache: &'a Arc<CsvCache>,
    pub recording_store: &'a Arc<RecordingStore>,
    pub recording_signature_headers: &'a [(String, String)],
    pub flow_state_configured: bool,
}

/// Extracted request metadata
#[derive(Clone)]
pub struct RequestInfo {
    pub method: hyper::Method,
    pub uri: hyper::Uri,
    pub headers: hyper::HeaderMap,
}

impl RequestInfo {
    pub fn from_request<B>(req: &Request<B>) -> Self {
        Self {
            method: req.method().clone(),
            uri: req.uri().clone(),
            headers: req.headers().clone(),
        }
    }
}

/// Reverse-proxy upstream service
#[derive(Clone, Default)]
pub struct UpstreamService {
    pub url: Option<String>,
    pub name: Option<String>,
}

pub struct ScriptingContext<'a> {
    pub compiled_scripts: &'a [(CompiledScript, CompiledRule, Option<String>)],
    pub script_pool: &'a Arc<ScriptPool>,
    pub decision_cache: &'a Arc<DecisionCache>,
}

pub struct ForwardingContext {
    pub info: RequestInfo,
    pub body_bytes: Bytes,
    pub upstream_service: UpstreamService,
    pub start_time: std::time::Instant,
}
