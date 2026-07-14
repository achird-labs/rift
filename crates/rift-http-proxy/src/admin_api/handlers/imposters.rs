//! Imposter CRUD handlers.

use crate::admin_api::request_filter::{parse_match_clauses, parse_since, request_matches};
use crate::admin_api::types::{
    ImposterDetail, ImposterListEntry, ImposterQueryParams, ImposterSummary, ListImpostersResponse,
    RiftImposterExtensions, StubWithLinks, build_response_with_headers, collect_body,
    error_response, json_response, make_imposter_links, make_stub_links,
};
use crate::extensions::decorate::backend_error_response;
use crate::imposter::RecordedRequest;
use crate::imposter::{
    Imposter, ImposterConfig, ImposterError, ImposterManager, Predicate, PredicateOperation,
    ScriptBaseDir, Stub, StubResponse, VerifyOptions, resolve_scripts,
};
use crate::scripting::validate_stubs;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// The `ScriptBaseDir` a `--scripts-dir`-carrying admin API resolves `file:` scripts under;
/// `Unconfigured` when the flag was never set. Shared by the imposter CRUD handlers and the stub
/// sub-resource handlers (issue #356 B1) so a `file:`/`ref:` script added through ANY admin-API
/// write path is resolved & escape-checked under the same root before it is persisted.
pub(crate) fn admin_script_base(scripts_dir: &Option<Arc<PathBuf>>) -> ScriptBaseDir {
    match scripts_dir {
        Some(dir) => ScriptBaseDir::ScriptsDir(dir.as_ref().clone()),
        None => ScriptBaseDir::Unconfigured,
    }
}

/// The `_rift.scripts` registry of the imposter on `port` (already resolved when the imposter was
/// created), for resolving a newly-added stub's `{ "ref": "name" }` against. Empty when the
/// imposter doesn't exist or declares no registry.
pub(crate) fn imposter_script_registry(
    manager: &ImposterManager,
    port: u16,
) -> std::collections::HashMap<String, crate::imposter::RiftScriptConfig> {
    manager
        .get_imposter(port)
        .ok()
        .and_then(|imposter| imposter.config.rift.as_ref().map(|r| r.scripts.clone()))
        .unwrap_or_default()
}

/// True if any stub in `stubs` uses a Mountebank scripting surface gated by `--allowInjection`
/// (issue #355 Item 4): an inject response, a decorate behavior (`_behaviors.decorate` / a
/// proxy's `addDecorateBehavior`), a `_behaviors.shellTransform` (runs a host shell command),
/// a `wait` behavior expressed as a JS function (which this engine now executes on Boa), a
/// predicate `inject`, a `predicateGenerators.inject`, or `_rift.script`. Mirrors Mountebank's
/// `allowInjection` gate.
fn stubs_contain_script_surface(stubs: &[Stub]) -> bool {
    stubs.iter().any(|stub| {
        stub.predicates.iter().any(predicate_has_inject)
            || stub.responses.iter().any(response_has_script_surface)
    })
}

/// True if `predicate` (or anything nested under a `not`/`or`/`and`) is an `inject` predicate.
fn predicate_has_inject(predicate: &Predicate) -> bool {
    match &predicate.operation {
        PredicateOperation::Inject(_) => true,
        PredicateOperation::Not(inner) => predicate_has_inject(inner),
        PredicateOperation::Or(preds) | PredicateOperation::And(preds) => {
            preds.iter().any(predicate_has_inject)
        }
        _ => false,
    }
}

/// True if `response` uses any script surface: an inject response, a decorate behavior, a
/// shellTransform behavior, a JS-function `wait` behavior, or `_rift.script`.
fn response_has_script_surface(response: &StubResponse) -> bool {
    match response {
        StubResponse::Inject { .. } => true,
        StubResponse::RiftScript { rift } => rift.script.is_some(),
        StubResponse::Is {
            behaviors, rift, ..
        } => {
            let behavior_is_scripted = behaviors
                .as_ref()
                .and_then(|b| {
                    serde_json::from_value::<crate::behaviors::ResponseBehaviors>(b.clone()).ok()
                })
                .is_some_and(|b| behaviors_contain_script_surface(&b));
            behavior_is_scripted || rift.as_ref().is_some_and(|r| r.script.is_some())
        }
        StubResponse::Proxy { proxy } => {
            proxy.add_decorate_behavior.is_some()
                || proxy
                    .predicate_generators
                    .iter()
                    .any(|g| g.get("inject").and_then(|v| v.as_str()).is_some())
        }
        StubResponse::Fault { .. } => false,
    }
}

/// True if the parsed `_behaviors` block carries a scripting surface: `decorate` (JS/Rhai),
/// `shellTransform` (runs a host shell command — B1), or a `wait` expressed as a JS function
/// (executed on Boa since issue #355 Item 6 — B2). A numeric `wait` (`Fixed`/`Range`) is NOT a
/// scripting surface and stays allowed.
fn behaviors_contain_script_surface(behaviors: &crate::behaviors::ResponseBehaviors) -> bool {
    behaviors.decorate.is_some()
        || !behaviors.shell_transform.is_empty()
        || matches!(
            behaviors.wait,
            Some(crate::behaviors::WaitBehavior::Function(_))
        )
}

/// Reject a set of stubs carrying a Mountebank scripting surface when `--allowInjection` is off,
/// mirroring Mountebank's gate (issue #355 Item 4). `None` when the stubs are allowed through.
/// Shared by the imposter CRUD handlers and the stub sub-resource handlers (B3) so the gate can't
/// be bypassed by adding a script-bearing stub through `POST/PUT /imposters/:port/stubs[...]`.
pub(crate) fn reject_stubs_if_injection_disallowed(
    stubs: &[Stub],
    allow_injection: bool,
) -> Option<Response<Full<Bytes>>> {
    if allow_injection || !stubs_contain_script_surface(stubs) {
        return None;
    }
    Some(injection_disallowed_response())
}

/// The Mountebank-compatible `400 invalid injection` response returned when a request carries a
/// scripting surface (`inject`, decorate, …) but `--allowInjection` is off.
fn injection_disallowed_response() -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "errors": [{
            "code": "invalid injection",
            "message": "inject requires --allowInjection to be set. See \
                        http://www.mbtest.org/docs/api/injection for more information.",
        }]
    })
    .to_string();
    build_response_with_headers(
        StatusCode::BAD_REQUEST,
        [("Content-Type", "application/json")],
        body,
    )
}

/// Reject a whole imposter config when its stubs carry a scripting surface and `--allowInjection`
/// is off. Delegates to [`reject_stubs_if_injection_disallowed`].
fn reject_if_injection_disallowed(
    config: &ImposterConfig,
    allow_injection: bool,
) -> Option<Response<Full<Bytes>>> {
    reject_stubs_if_injection_disallowed(&config.stubs, allow_injection)
}

/// POST /imposters - Create a new imposter
pub async fn handle_create(
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };

    let mut config: ImposterConfig = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid imposter JSON: {e}"),
            );
        }
    };

    if let Some(rejection) = reject_if_injection_disallowed(&config, allow_injection) {
        return rejection;
    }

    // Resolve `_rift.script` `file:`/`ref:` sources (issue #356) before validating/creating —
    // a `file:` outside `--scripts-dir` (or with no `--scripts-dir` configured at all) is
    // rejected here, never read.
    if let Err(e) = resolve_scripts(&mut config, &admin_script_base(&scripts_dir)) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("Script resolution failed: {e}"),
        );
    }

    // Validate all scripts in stubs before creating the imposter
    let validation_result = validate_stubs(&config.stubs);
    if !validation_result.is_valid() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Script validation failed: {}",
                validation_result.into_error_message().unwrap_or_default()
            ),
        );
    }

    match manager.create_imposter(config).await {
        Ok(assigned_port) => {
            info!("Created imposter on port {}", assigned_port);
            // Return the full imposter details with 201 Created
            let response = handle_get(assigned_port, None, base_url, manager).await;
            let (parts, body) = response.into_parts();
            let mut new_parts = parts;
            new_parts.status = StatusCode::CREATED;
            Response::from_parts(new_parts, body)
        }
        Err(e) => e.into(),
    }
}

/// GET /imposters - List all imposters
pub async fn handle_list(
    manager: Arc<ImposterManager>,
    query: Option<&str>,
    base_url: &str,
) -> Response<Full<Bytes>> {
    let params = ImposterQueryParams::parse(query);
    let imposters = manager.list_imposters();

    if params.replayable {
        let configs: Vec<ImposterConfig> = imposters
            .iter()
            .map(|i| {
                if params.remove_proxies {
                    filter_proxy_responses(&i.config)
                } else {
                    i.config.clone()
                }
            })
            .collect();
        let body = serde_json::json!({ "imposters": configs });
        json_response(StatusCode::OK, &body)
    } else if params.list {
        // Mountebank-compatible abbreviated listing: port, protocol, name, numberOfRequests, _links
        let entries: Vec<ImposterListEntry> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterListEntry {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    number_of_requests: i.get_request_count(),
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();
        json_response(StatusCode::OK, &serde_json::json!({ "imposters": entries }))
    } else {
        let summaries: Vec<ImposterSummary> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterSummary {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    number_of_requests: i.get_request_count(),
                    stub_count: i.stub_count(),
                    enabled: i.is_enabled(),
                    record_requests: i.config.record_requests,
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();

        let response = ListImpostersResponse {
            imposters: summaries,
        };
        json_response(StatusCode::OK, &response)
    }
}

/// PUT /imposters - Replace all imposters
pub async fn handle_replace_all(
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    replace_all_from_bytes(&body, base_url, manager, allow_injection, scripts_dir).await
}

/// Parse, validate, and apply a `PUT /imposters` batch. Split out from `handle_replace_all` so
/// the path is unit-testable without a `Request<Incoming>` (same seam as `verify_response`).
async fn replace_all_from_bytes(
    body: &[u8],
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    #[derive(Deserialize)]
    struct BatchRequest {
        imposters: Vec<ImposterConfig>,
    }

    let mut batch: BatchRequest = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid batch JSON: {e}"));
        }
    };

    // Reject the whole batch (before making any changes) if any imposter carries a script
    // surface and --allowInjection is off (issue #355 Item 4).
    for config in &batch.imposters {
        if let Some(rejection) = reject_if_injection_disallowed(config, allow_injection) {
            return rejection;
        }
    }

    // Resolve `_rift.script` `file:`/`ref:` sources (issue #356) for every imposter before
    // making any changes — same rejection rules as `handle_create`.
    let base = admin_script_base(&scripts_dir);
    for (idx, config) in batch.imposters.iter_mut().enumerate() {
        if let Err(e) = resolve_scripts(config, &base) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Script resolution failed in imposter[{idx}] (port {:?}): {e}",
                    config.port
                ),
            );
        }
    }

    // Validate all scripts in all imposters before making any changes
    for (idx, config) in batch.imposters.iter().enumerate() {
        let validation_result = validate_stubs(&config.stubs);
        if !validation_result.is_valid() {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Script validation failed in imposter[{}] (port {:?}): {}",
                    idx,
                    config.port,
                    validation_result.into_error_message().unwrap_or_default()
                ),
            );
        }
    }

    // Reconcile toward the new set instead of delete-all-then-recreate (issue #549): the old
    // wholesale pre-delete meant any create failure (e.g. a transiently-held port) silently lost
    // the previous imposters behind a 200. apply_config validates the whole set before touching
    // anything, only replaces what actually changed (an unchanged imposter keeps its runtime
    // state, like POST /admin/reload), and reports per-port failures instead of log-and-continue.
    match manager.apply_config(batch.imposters).await {
        Ok(report) if report.failed.is_empty() => handle_list(manager, None, base_url).await,
        // Residual per-port apply failures (e.g. a bind race): the other ports are already
        // reconciled, so carry the full report — a partial failure is exactly when the client
        // needs to know what did apply (same contract as POST /admin/reload).
        Ok(report) => {
            let failures: Vec<String> = report
                .failed
                .iter()
                .map(|(port, e)| match port {
                    // Port 0 is the ApplyReport sentinel for auto-assigned (port-less) configs.
                    0 => format!("auto-assign: {e}"),
                    port => format!("{port}: {e}"),
                })
                .collect();
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({
                    "errors": [{
                        "code": "500",
                        "message": format!(
                            "Replace partially failed: {}",
                            failures.join("; ")
                        ),
                    }],
                    "failed": failures,
                    "created": report.created,
                    "replaced": report.replaced,
                    "stubPatched": report.stub_patched,
                    "deleted": report.deleted,
                }),
            )
        }
        // Set-level validation failure (bad protocol, duplicate port, duplicate stub id): a bad
        // request body, rejected before any imposter was touched.
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &format!("Invalid imposter set (imposters unchanged): {e}"),
        ),
    }
}

/// DELETE /imposters - Delete all imposters
pub async fn handle_delete_all(
    manager: Arc<ImposterManager>,
    _base_url: &str,
) -> Response<Full<Bytes>> {
    let configs = manager.delete_all().await;
    let body = serde_json::json!({ "imposters": configs });
    json_response(StatusCode::OK, &body)
}

/// GET /imposters/:port - Get a specific imposter
pub async fn handle_get(
    port: u16,
    query: Option<&str>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let params = ImposterQueryParams::parse(query);

    match manager.get_imposter(port) {
        Ok(imposter) => {
            let mut stubs = imposter.get_stubs();

            if params.remove_proxies {
                stubs = filter_proxy_stubs(stubs);
            }

            // Cached stub-overlap analysis (issue #423): computed once in core on stub mutation,
            // not recomputed here on every read. Reflects the imposter's real stubs regardless of
            // the `removeProxies` view (the warnings are advisory).
            let warnings = imposter.stub_warnings();
            if !warnings.is_empty() {
                for warning in warnings.iter() {
                    warn!(
                        port = port,
                        warning_type = ?warning.warning_type,
                        "Stub analysis warning: {}",
                        warning.message
                    );
                }
            }
            // Expose the imposter's flow-state config (issue #260) so tools can read the
            // correlated-isolation `flowIdSource`.
            let flow_state = imposter
                .config
                .rift
                .as_ref()
                .and_then(|r| r.flow_state.as_ref())
                .map(expose_flow_state);
            let rift_extensions = if !warnings.is_empty() || flow_state.is_some() {
                Some(RiftImposterExtensions {
                    warnings: (*warnings).clone(),
                    flow_state,
                })
            } else {
                None
            };

            let stubs_with_links: Vec<StubWithLinks> = stubs
                .into_iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub,
                    links: make_stub_links(base_url, port, index),
                })
                .collect();

            let detail = ImposterDetail {
                protocol: imposter.config.protocol.clone(),
                port: imposter.config.port.unwrap_or(port),
                name: imposter.config.name.clone(),
                number_of_requests: imposter.get_request_count(),
                enabled: imposter.is_enabled(),
                record_requests: imposter.config.record_requests,
                requests: imposter.get_recorded_requests(),
                stubs: stubs_with_links,
                links: make_imposter_links(base_url, port),
                rift: rift_extensions,
            };
            json_response(StatusCode::OK, &detail)
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port - Delete a specific imposter
pub async fn handle_delete(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_imposter(port).await {
        Ok(config) => {
            info!("Deleted imposter on port {}", port);
            let stubs_with_links: Vec<StubWithLinks> = config
                .stubs
                .iter()
                .enumerate()
                .map(|(index, stub)| StubWithLinks {
                    stub: stub.clone(),
                    links: make_stub_links(base_url, port, index),
                })
                .collect();
            let response = serde_json::json!({
                "protocol": config.protocol,
                "port": config.port,
                "name": config.name,
                "numberOfRequests": 0,
                "recordRequests": config.record_requests,
                "requests": [],
                "stubs": stubs_with_links,
                "_links": make_imposter_links(base_url, port)
            });
            json_response(StatusCode::OK, &response)
        }
        Err(ImposterError::NotFound(_)) => error_response(
            StatusCode::NOT_FOUND,
            &format!("No imposter exists on port {port}"),
        ),
        Err(e) => e.into(),
    }
}

/// POST /imposters/:port/enable - Enable imposter
pub async fn handle_enable(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    handle_set_enabled(port, true, manager).await
}

/// POST /imposters/:port/disable - Disable imposter
pub async fn handle_disable(port: u16, manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    handle_set_enabled(port, false, manager).await
}

/// Set enabled state for an imposter
async fn handle_set_enabled(
    port: u16,
    enabled: bool,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.set_enabled(enabled);
            let state = if enabled { "enabled" } else { "disabled" };
            json_response(
                StatusCode::OK,
                &serde_json::json!({"message": format!("Imposter {}", state)}),
            )
        }
        Err(e) => e.into(),
    }
}

/// Build the public projection of a `flowState` for `GET /imposters` (issue #260). Fail-closed
/// allowlist: only the non-sensitive fields tools need are exposed, so a credential-bearing field
/// added to the config later (e.g. inside `redis`, or a new backend's auth block) is excluded by
/// default rather than leaked.
fn expose_flow_state(fs: &rift_mock_core::imposter::RiftFlowStateConfig) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("backend".to_string(), serde_json::json!(fs.backend));
    out.insert("ttlSeconds".to_string(), serde_json::json!(fs.ttl_seconds));
    if let Some(source) = &fs.flow_id_source {
        out.insert("flowIdSource".to_string(), serde_json::json!(source));
    }
    serde_json::Value::Object(out)
}

/// Resolve the imposter's `flow_id_source` (default `"imposter_port"`).
fn flow_id_source(imposter: &Imposter) -> String {
    imposter
        .config
        .rift
        .as_ref()
        .and_then(|r| r.flow_state.as_ref())
        .and_then(|fs| fs.flow_id_source.clone())
        .unwrap_or_else(|| "imposter_port".to_string())
}

/// GET /imposters/:port/requests[?match=...][?since=<index>] - recorded requests, optionally
/// filtered by `header:<Name>=<Value>` / `flow_id=<Value>` clauses (AND'd) and cut to entries
/// newer than a cursor.
///
/// The body is always the historical bare array; cursor metadata rides in `x-rift-next-index`
/// (and `x-rift-truncated` when retention outran the caller). Backends without stable indices
/// emit neither, which is exactly how an SDK probes for cursor support (issue #603).
pub async fn handle_get_requests(
    port: u16,
    query: Option<&str>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let clauses = match parse_match_clauses(query) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let since = match parse_since(query) {
        Ok(s) => s,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let source = flow_id_source(&imposter);
            // Filter over references before cloning so an unmatched journal entry is never
            // deep-cloned (issue #485) — on both the cursor and the fallback path.
            let keep = |r: &RecordedRequest| request_matches(r, &clauses, &source, port);
            match imposter.read_recorded_requests_since(since, keep) {
                Some(read) => {
                    let entries: Vec<RecordedRequest> =
                        read.entries.into_iter().map(|e| e.request).collect();
                    if read.complete {
                        cursor_response(&entries, read.next, read.truncated)
                    } else {
                        // A degraded read reached only part of its storage, so `next` spans
                        // entries that were never served. Withholding the cursor is what stops
                        // a client from advancing past them and losing them for good; it
                        // re-polls instead, exactly as against a backend with no cursor at all.
                        json_response(StatusCode::OK, &entries)
                    }
                }
                None => {
                    let filtered = imposter.get_recorded_requests_filtered(keep);
                    json_response(StatusCode::OK, &filtered)
                }
            }
        }
        Err(e) => e.into(),
    }
}

/// The savedRequests array plus its cursor headers. `truncated` is emitted only when true —
/// an SDK tests for the header's presence, never parses a `false`.
fn cursor_response(
    entries: &[RecordedRequest],
    next: u64,
    truncated: bool,
) -> Response<Full<Bytes>> {
    let json = match serde_json::to_string_pretty(entries) {
        Ok(j) => j,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to serialize recorded requests: {e}"),
            );
        }
    };
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-rift-next-index".to_string(), next.to_string()),
    ];
    if truncated {
        headers.push(("x-rift-truncated".to_string(), "true".to_string()));
    }
    build_response_with_headers(StatusCode::OK, headers, json)
}

/// DELETE /imposters/:port/savedRequests - Clear recorded requests.
/// With `match=...` clauses, only the matching slice is removed (the rest are kept);
/// without clauses, all recorded requests are cleared.
pub async fn handle_clear_requests(
    port: u16,
    query: Option<&str>,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let clauses = match parse_match_clauses(query) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            if clauses.is_empty() {
                if let Err(e) = imposter.clear_recorded_requests() {
                    return backend_error_response(&e);
                }
            } else {
                let source = flow_id_source(&imposter);
                imposter.retain_recorded_requests(|r| !request_matches(r, &clauses, &source, port));
            }
            handle_get(port, None, base_url, manager).await
        }
        Err(e) => e.into(),
    }
}

/// POST /imposters/:port/verify - count (and optionally return) recorded requests matching a
/// predicate set, with an optional closest non-match diff (issue #494). The engine owns the one
/// true predicate evaluator, so every SDK's `verify(match, times(n))` defers here instead of
/// re-implementing matching (and shipping the whole journal over the wire just to count it).
pub async fn handle_verify(
    port: u16,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    // An `inject` predicate evaluates synchronous Boa JavaScript that can run away (issue #476);
    // evaluate the whole verify on the blocking pool so it never head-of-line-blocks a tokio
    // worker. Like the live matcher (#476) there's no abort flag — Boa's loop-iteration cap (#327)
    // eventually frees the blocking thread. A non-inject verify pays only a cheap task hop, fine
    // since verify is not a per-request hot path.
    tokio::task::spawn_blocking(move || verify_response(port, &body, &manager, allow_injection))
        .await
        .unwrap_or_else(|e| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("verify task failed: {e}"),
            )
        })
}

/// The body of [`handle_verify`] over already-collected bytes, so the parse/gate/evaluate path is
/// unit-testable without a `Request<Incoming>`.
fn verify_response(
    port: u16,
    body: &[u8],
    manager: &ImposterManager,
    allow_injection: bool,
) -> Response<Full<Bytes>> {
    let opts: VerifyOptions = match serde_json::from_slice(body) {
        Ok(o) => o,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid verify JSON: {e}"),
            );
        }
    };
    // An `inject` predicate evaluates Boa JavaScript — a scripting surface gated by
    // `--allowInjection` for untrusted admin clients (issue #355), the same gate the stub
    // endpoints apply before accepting an inject predicate.
    if !allow_injection && opts.predicates.iter().any(predicate_has_inject) {
        return injection_disallowed_response();
    }
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.verify(&opts) {
            Ok(outcome) => json_response(StatusCode::OK, &outcome),
            // A failing `inject` predicate is the only error path (issue #440); it is caused by the
            // caller-supplied predicate, so it maps to 400 rather than a server fault.
            Err(e) => error_response(StatusCode::BAD_REQUEST, &e.to_string()),
        },
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/savedProxyResponses - Clear proxy responses
pub async fn handle_clear_proxy_responses(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            imposter.clear_proxy_responses();
            handle_get(port, None, base_url, manager).await
        }
        Err(e) => e.into(),
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// Filter out proxy responses from stubs. `pub` (not just `pub(crate)`) so the FFI layer
/// (issue #491) can apply the SAME `removeProxies` projection the admin handlers use, instead of
/// re-implementing it and risking drift.
pub fn filter_proxy_responses(config: &ImposterConfig) -> ImposterConfig {
    let mut filtered = config.clone();
    filtered.stubs = filter_proxy_stubs(config.stubs.clone());
    filtered
}

/// Filter proxy responses from a list of stubs. `pub` so the FFI `rift_get_imposter` detail view
/// (issue #491) applies the SAME `removeProxies` projection this crate's `handle_get` applies to
/// its live `get_stubs()`, rather than re-implementing it.
pub fn filter_proxy_stubs(stubs: Vec<crate::imposter::Stub>) -> Vec<crate::imposter::Stub> {
    stubs
        .into_iter()
        .filter_map(|stub| {
            let non_proxy_responses: Vec<_> = stub
                .responses
                .iter()
                .filter(|r| !matches!(r, StubResponse::Proxy { .. }))
                .cloned()
                .collect();

            if non_proxy_responses.is_empty() {
                None
            } else {
                Some(crate::imposter::Stub {
                    scenario_name: stub.scenario_name,
                    required_scenario_state: stub.required_scenario_state,
                    new_scenario_state: stub.new_scenario_state,
                    space: stub.space,
                    id: stub.id,
                    route_pattern: stub.route_pattern,
                    predicates: stub.predicates,
                    responses: non_proxy_responses,
                    recorded_from: stub.recorded_from,
                    verify: stub.verify,
                })
            }
        })
        .collect()
}

// Issue #355 Item 4: `--allowInjection` gates any Mountebank scripting surface on
// POST/PUT /imposters. Exercised at the `reject_if_injection_disallowed` level (the exact
// decision `handle_create`/`handle_replace_all` apply) rather than through a live HTTP request,
// since building a real `hyper::body::Incoming` outside a running server is impractical in a
// `--lib` unit test; the handlers above are a thin `if let Some(rejection) = ... { return }`
// wrapper around this function.
#[cfg(test)]
mod allow_injection_tests {
    use super::*;
    use serde_json::json;

    fn cfg(value: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(value).expect("test imposter config")
    }

    #[test]
    fn non_script_imposter_always_accepted() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/ok" } }],
                "responses": [{ "is": { "statusCode": 200, "body": "fine" } }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_none());
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    #[test]
    fn inject_response_rejected_when_disallowed_allowed_when_enabled() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{ "inject": "function(config) { return { statusCode: 200 }; }" }]
            }]
        }));
        let rejection = reject_if_injection_disallowed(&config, false)
            .expect("inject response must be rejected when allowInjection is off");
        assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    #[test]
    fn decorate_behavior_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "decorate": "function(config) { }" }
                }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    #[test]
    fn proxy_add_decorate_behavior_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "proxy": { "to": "http://example.com", "addDecorateBehavior": "function(config) {}" }
                }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
    }

    #[test]
    fn predicate_inject_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [{ "inject": "function(config) { return true; }" }],
                "responses": [{ "is": { "statusCode": 200 } }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    #[test]
    fn predicate_inject_nested_under_not_is_still_detected() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [{ "not": { "inject": "function(config) { return true; }" } }],
                "responses": [{ "is": { "statusCode": 200 } }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
    }

    #[test]
    fn predicate_generator_inject_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "proxy": {
                        "to": "http://example.com",
                        "predicateGenerators": [{ "inject": "function(config, logger, preds) { return preds; }" }]
                    }
                }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    #[test]
    fn rift_script_response_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "javascript", "code": "function f(){}" } }
                }]
            }]
        }));
        assert!(reject_if_injection_disallowed(&config, false).is_some());
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    // B1: shellTransform runs a host shell command and MUST be gated.
    #[test]
    fn shell_transform_rejected_when_disallowed_allowed_when_enabled() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "shellTransform": "cat" }
                }]
            }]
        }));
        assert!(
            reject_if_injection_disallowed(&config, false).is_some(),
            "shellTransform is a host-command execution surface and must be gated"
        );
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    // B2: a `wait` expressed as a JS function is now executed on Boa, so it's an injection surface.
    #[test]
    fn wait_function_rejected_when_disallowed() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "wait": "function() { return 10; }" }
                }]
            }]
        }));
        assert!(
            reject_if_injection_disallowed(&config, false).is_some(),
            "a JS-function wait is executed on Boa and must be gated"
        );
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    // B2: a numeric wait (Fixed/Range) is NOT a scripting surface and must stay allowed.
    #[test]
    fn numeric_wait_always_allowed() {
        let fixed = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "wait": 250 }
                }]
            }]
        }));
        assert!(
            reject_if_injection_disallowed(&fixed, false).is_none(),
            "a numeric (Fixed) wait must never be gated"
        );

        let range = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200, "body": "x" },
                    "_behaviors": { "wait": { "min": 100, "max": 200 } }
                }]
            }]
        }));
        assert!(
            reject_if_injection_disallowed(&range, false).is_none(),
            "a numeric (Range) wait must never be gated"
        );
    }

    // B3: the shared stub-slice gate rejects a script-bearing single stub when off, and the same
    // gate the stub sub-resource handlers call.
    #[test]
    fn reject_stubs_gate_rejects_script_bearing_slice() {
        let config = cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{ "inject": "function(config) { return { statusCode: 200 }; }" }]
            }]
        }));
        let stubs = &config.stubs;
        assert!(
            reject_stubs_if_injection_disallowed(stubs, false).is_some(),
            "the shared stub-slice gate must reject a script-bearing stub when injection is off"
        );
        assert!(reject_stubs_if_injection_disallowed(stubs, true).is_none());

        let clean = cfg(json!({
            "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }));
        assert!(
            reject_stubs_if_injection_disallowed(&clean.stubs, false).is_none(),
            "a non-script stub slice is always allowed"
        );
    }
}

// Issue #201: filter recorded requests by header / flow_id via
// GET /imposters/:port/requests?match=...  (and targeted DELETE).
#[cfg(test)]
mod redact_tests {
    use super::expose_flow_state;
    use serde_json::json;

    #[test]
    fn expose_flow_state_allowlists_safe_fields_only() {
        let fs: rift_mock_core::imposter::RiftFlowStateConfig = serde_json::from_value(json!({
            "backend": "redis",
            "ttlSeconds": 300,
            "redis": { "url": "redis://user:secret@host:6379", "keyPrefix": "rift:" },
            "flowIdSource": "header:X-Mock-Space"
        }))
        .unwrap();
        let out = expose_flow_state(&fs);
        assert!(
            out.get("redis").is_none(),
            "redis (credentialed URL) must never be exposed"
        );
        assert_eq!(
            out.get("flowIdSource").and_then(|v| v.as_str()),
            Some("header:X-Mock-Space")
        );
        assert_eq!(out.get("backend").and_then(|v| v.as_str()), Some("redis"));
        // The credential string must not survive anywhere in the exposed value.
        assert!(!out.to_string().contains("secret"));
    }
}

#[cfg(test)]
mod requests_filter_tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::collections::HashMap;

    fn rec(space: &str, tenant: Option<&str>) -> RecordedRequest {
        let mut headers = HashMap::new();
        headers.insert("X-Mock-Space".to_string(), vec![space.to_string()]);
        if let Some(t) = tenant {
            headers.insert("X-Tenant".to_string(), vec![t.to_string()]);
        }
        RecordedRequest {
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/p".to_string(),
            query: HashMap::new(),
            headers,
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    async fn manager_with(
        port: u16,
        flow_id_source: Option<&str>,
        recorded: &[RecordedRequest],
    ) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        let mut cfg = serde_json::json!({
            "port": port, "protocol": "http", "recordRequests": true, "stubs": []
        });
        if let Some(src) = flow_id_source {
            cfg["_rift"] = serde_json::json!({
                "flowState": { "flowIdSource": src }
            });
        }
        let config = serde_json::from_value(cfg).expect("config");
        manager.create_imposter(config).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r);
        }
        manager
    }

    async fn requests(resp: Response<Full<Bytes>>) -> Vec<RecordedRequest> {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("decode requests")
    }

    #[tokio::test]
    async fn get_requests_returns_all_without_match() {
        let m = manager_with(19731, None, &[rec("A", None), rec("B", None)]).await;
        let resp = handle_get_requests(19731, None, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(requests(resp).await.len(), 2);
        let _ = m.delete_imposter(19731).await;
    }

    #[tokio::test]
    async fn get_requests_filters_by_header() {
        let m = manager_with(
            19732,
            None,
            &[rec("A", None), rec("B", None), rec("A", None)],
        )
        .await;
        let resp = handle_get_requests(19732, Some("match=header:X-Mock-Space=A"), m.clone()).await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 2, "only the two X-Mock-Space=A requests");
        assert!(got.iter().all(|r| {
            r.headers
                .get("X-Mock-Space")
                .and_then(|v| v.first())
                .map(String::as_str)
                == Some("A")
        }));
        let _ = m.delete_imposter(19732).await;
    }

    #[tokio::test]
    async fn get_requests_no_match_is_empty_200() {
        let m = manager_with(19733, None, &[rec("A", None)]).await;
        let resp =
            handle_get_requests(19733, Some("match=header:X-Mock-Space=ZZZ"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(requests(resp).await.is_empty());
        let _ = m.delete_imposter(19733).await;
    }

    #[tokio::test]
    async fn get_requests_flow_id_header_source_parity() {
        let m = manager_with(
            19734,
            Some("header:X-Mock-Space"),
            &[rec("A", None), rec("B", None)],
        )
        .await;
        let by_flow_id =
            requests(handle_get_requests(19734, Some("match=flow_id=A"), m.clone()).await).await;
        let by_header = requests(
            handle_get_requests(19734, Some("match=header:X-Mock-Space=A"), m.clone()).await,
        )
        .await;
        assert_eq!(by_flow_id.len(), 1);
        assert_eq!(by_flow_id[0].headers.get("X-Mock-Space").unwrap()[0], "A");
        assert_eq!(
            serde_json::to_value(&by_flow_id).unwrap(),
            serde_json::to_value(&by_header).unwrap(),
            "flow_id= and header: must return the identical slice when flow_id_source is that header"
        );
        let _ = m.delete_imposter(19734).await;
    }

    #[tokio::test]
    async fn get_requests_flow_id_imposter_port() {
        let m = manager_with(19735, None, &[rec("A", None), rec("B", None)]).await;
        // default flow_id_source = imposter_port → every request shares flow_id = port
        let all = handle_get_requests(19735, Some("match=flow_id=19735"), m.clone()).await;
        assert_eq!(requests(all).await.len(), 2);
        let none = handle_get_requests(19735, Some("match=flow_id=9999"), m.clone()).await;
        assert!(requests(none).await.is_empty());
        let _ = m.delete_imposter(19735).await;
    }

    #[tokio::test]
    async fn get_requests_multiple_match_anded() {
        let m = manager_with(
            19736,
            None,
            &[
                rec("A", Some("t1")),
                rec("A", Some("t2")),
                rec("B", Some("t1")),
            ],
        )
        .await;
        let resp = handle_get_requests(
            19736,
            Some("match=header:X-Mock-Space=A&match=header:X-Tenant=t1"),
            m.clone(),
        )
        .await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 1, "only the A+t1 request matches both clauses");
        let _ = m.delete_imposter(19736).await;
    }

    #[tokio::test]
    async fn get_requests_unknown_port_404() {
        let m = Arc::new(ImposterManager::new());
        let resp = handle_get_requests(19737, None, m).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_requests_malformed_match_400() {
        let m = manager_with(19738, None, &[rec("A", None)]).await;
        let resp = handle_get_requests(19738, Some("match=path=/foo"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let _ = m.delete_imposter(19738).await;
    }

    #[tokio::test]
    async fn delete_requests_targeted_clear_removes_only_slice() {
        let m = manager_with(
            19739,
            None,
            &[rec("A", None), rec("B", None), rec("A", None)],
        )
        .await;
        let base = "http://localhost:2525";
        handle_clear_requests(19739, Some("match=header:X-Mock-Space=A"), base, m.clone()).await;
        let remaining = requests(handle_get_requests(19739, None, m.clone()).await).await;
        assert_eq!(remaining.len(), 1, "only the B request survives");
        assert_eq!(remaining[0].headers.get("X-Mock-Space").unwrap()[0], "B");
        let _ = m.delete_imposter(19739).await;
    }

    #[tokio::test]
    async fn delete_requests_without_match_clears_all() {
        let m = manager_with(19740, None, &[rec("A", None), rec("B", None)]).await;
        let base = "http://localhost:2525";
        handle_clear_requests(19740, None, base, m.clone()).await;
        let remaining = requests(handle_get_requests(19740, None, m.clone()).await).await;
        assert!(remaining.is_empty());
        let _ = m.delete_imposter(19740).await;
    }

    // A journal whose full clear fails like an unreachable remote backend (issue #330);
    // record/read delegate to a working LocalJournal so the imposter is otherwise normal.
    #[derive(Default)]
    struct FailingClearJournal(crate::imposter::LocalJournal);
    impl crate::imposter::RequestJournal for FailingClearJournal {
        fn note_request(&self, port: u16) {
            self.0.note_request(port);
        }
        fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
            self.0.record(port, flow_id, req);
        }
        fn read(&self, port: u16) -> crate::imposter::JournalRead {
            self.0.read(port)
        }
        fn clear(&self, _port: u16) -> anyhow::Result<()> {
            Err(anyhow::Error::new(
                crate::extensions::decorate::BackendUnavailable {
                    feature: "requestJournal",
                    detail: "clear failed".to_string(),
                },
            ))
        }
        fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
            self.0.retain(port, keep);
        }
        fn clear_flow(&self, _port: u16, _flow_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn count(&self, port: u16) -> u64 {
            self.0.count(port)
        }
    }

    // AC3 (#330): an unclearable backend makes DELETE savedRequests return a structured 503
    // rather than an unconditional 200 over stale data.
    #[tokio::test]
    async fn delete_requests_backend_clear_failure_maps_to_503() {
        let manager = Arc::new(
            ImposterManager::new().with_request_journal(Arc::new(FailingClearJournal::default())
                as Arc<dyn crate::imposter::RequestJournal>),
        );
        let cfg = serde_json::from_value(serde_json::json!({
            "port": 19742, "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        manager.create_imposter(cfg).await.expect("create");
        let base = "http://localhost:2525";
        let resp = handle_clear_requests(19742, None, base, manager.clone()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["error"], "backendUnavailable");
        let _ = manager.delete_imposter(19742).await;
    }

    #[tokio::test]
    async fn delete_requests_malformed_match_400() {
        let m = manager_with(19741, None, &[rec("A", None)]).await;
        let base = "http://localhost:2525";
        let resp = handle_clear_requests(19741, Some("match=path=/foo"), base, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // a rejected clear must leave the buffer untouched
        let remaining = requests(handle_get_requests(19741, None, m.clone()).await).await;
        assert_eq!(
            remaining.len(),
            1,
            "malformed DELETE must not clear anything"
        );
        let _ = m.delete_imposter(19741).await;
    }

    // ---- #603: savedRequests cursor ----------------------------------------------------

    fn header<'a>(resp: &'a Response<Full<Bytes>>, name: &str) -> Option<&'a str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    fn next_index(resp: &Response<Full<Bytes>>) -> Option<&str> {
        header(resp, "x-rift-next-index")
    }

    fn rec_at(path: &str, space: &str) -> RecordedRequest {
        let mut r = rec(space, None);
        r.path = path.to_string();
        r
    }

    // AC7 (#603): `?since=` serves only entries newer than the cursor, and reports the new
    // cursor in the response header. The body stays the historical bare array.
    #[tokio::test]
    async fn get_requests_since_returns_newer_with_cursor_header() {
        let m = manager_with(
            19750,
            None,
            &[rec_at("/a", "A"), rec_at("/b", "A"), rec_at("/c", "A")],
        )
        .await;

        let resp = handle_get_requests(19750, Some("since=1"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(next_index(&resp), Some("3"));
        let got = requests(resp).await;
        assert_eq!(
            got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/b", "/c"],
            "index 1 was already seen"
        );

        // Caught up: empty body, cursor unchanged.
        let resp = handle_get_requests(19750, Some("since=3"), m.clone()).await;
        assert_eq!(next_index(&resp), Some("3"));
        assert!(requests(resp).await.is_empty());

        let _ = m.delete_imposter(19750).await;
    }

    // AC8 (#603): `since` and `match` compose — cursor cut first, filter after — and a window
    // that matches nothing still advances the cursor.
    #[tokio::test]
    async fn get_requests_since_composes_with_match() {
        let m = manager_with(
            19751,
            None,
            &[
                rec_at("/1", "A"),
                rec_at("/2", "B"),
                rec_at("/3", "A"),
                rec_at("/4", "B"),
            ],
        )
        .await;

        let resp = handle_get_requests(
            19751,
            Some("since=1&match=header:X-Mock-Space=A"),
            m.clone(),
        )
        .await;
        assert_eq!(next_index(&resp), Some("4"), "cursor spans scanned entries");
        let got = requests(resp).await;
        assert_eq!(
            got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/3"],
            "cut at 1, then keep only space A"
        );

        // A filtered tail whose window matches nothing must still advance, or it re-scans forever.
        let resp = handle_get_requests(
            19751,
            Some("since=1&match=header:X-Mock-Space=ZZZ"),
            m.clone(),
        )
        .await;
        assert_eq!(next_index(&resp), Some("4"));
        assert!(requests(resp).await.is_empty());

        let _ = m.delete_imposter(19751).await;
    }

    // AC9 (#603): a malformed cursor is rejected before any journal work.
    #[tokio::test]
    async fn get_requests_rejects_non_numeric_since() {
        let m = manager_with(19752, None, &[rec_at("/a", "A")]).await;
        for bad in ["since=abc", "since=-1", "since=", "since=1.5"] {
            let resp = handle_get_requests(19752, Some(bad), m.clone()).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "'{bad}' must be rejected"
            );
            assert!(next_index(&resp).is_none(), "no cursor header on a 400");
        }
        let _ = m.delete_imposter(19752).await;
    }

    // A journal with no stable indices: it records and reads normally but never overrides the
    // cursor methods, so it inherits the honest `None` defaults (the out-of-tree embedder case).
    #[derive(Default)]
    struct CursorlessJournal(crate::imposter::LocalJournal);
    impl crate::imposter::RequestJournal for CursorlessJournal {
        fn note_request(&self, port: u16) {
            self.0.note_request(port);
        }
        fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
            self.0.record(port, flow_id, req);
        }
        fn read(&self, port: u16) -> crate::imposter::JournalRead {
            self.0.read(port)
        }
        fn clear(&self, port: u16) -> anyhow::Result<()> {
            self.0.clear(port)
        }
        fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
            self.0.retain(port, keep);
        }
        fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
            self.0.clear_flow(port, flow_id)
        }
        fn count(&self, port: u16) -> u64 {
            self.0.count(port)
        }
    }

    async fn manager_with_journal(
        port: u16,
        journal: Arc<dyn crate::imposter::RequestJournal>,
        recorded: &[RecordedRequest],
    ) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new().with_request_journal(journal));
        let cfg = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        manager.create_imposter(cfg).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r);
        }
        manager
    }

    // AC10 (#603): against a backend without stable indices the endpoint behaves exactly as it
    // does today — full list, no cursor headers. That absence IS the SDK capability probe, so
    // it must hold even when the client asks for a cursor.
    #[tokio::test]
    async fn get_requests_omits_cursor_headers_without_support() {
        let m = manager_with_journal(
            19753,
            Arc::new(CursorlessJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A")],
        )
        .await;

        for query in [None, Some("since=1")] {
            let resp = handle_get_requests(19753, query, m.clone()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(
                next_index(&resp).is_none(),
                "no cursor support => no cursor header (the probe)"
            );
            assert!(header(&resp, "x-rift-truncated").is_none());
            assert_eq!(
                requests(resp).await.len(),
                2,
                "an unsupported `since` is ignored, not an error — the full list is served"
            );
        }
        let _ = m.delete_imposter(19753).await;
    }

    // AC11 (#603): the baseline poll (no `since`) is unchanged in body but carries the cursor
    // the SDK loop starts from — including on an empty journal.
    #[tokio::test]
    async fn get_requests_baseline_reports_next_index() {
        let empty = manager_with(19754, None, &[]).await;
        let resp = handle_get_requests(19754, None, empty.clone()).await;
        assert_eq!(next_index(&resp), Some("0"), "0 == 'seen nothing yet'");
        assert!(header(&resp, "x-rift-truncated").is_none());
        assert!(requests(resp).await.is_empty());
        let _ = empty.delete_imposter(19754).await;

        let m = manager_with(19755, None, &[rec_at("/a", "A"), rec_at("/b", "A")]).await;
        let resp = handle_get_requests(19755, None, m.clone()).await;
        assert_eq!(next_index(&resp), Some("2"));
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "a baseline read is never truncated"
        );
        assert_eq!(requests(resp).await.len(), 2);
        let _ = m.delete_imposter(19755).await;
    }

    // A journal reporting a cursor whose unseen entries were lost to retention pressure.
    // Eviction semantics themselves are proven in journal.rs; this pins the handler's plumbing
    // of the flag onto the wire.
    #[derive(Default)]
    struct TruncatingJournal(crate::imposter::LocalJournal);
    impl crate::imposter::RequestJournal for TruncatingJournal {
        fn note_request(&self, port: u16) {
            self.0.note_request(port);
        }
        fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
            self.0.record(port, flow_id, req);
        }
        fn record_indexed(&self, port: u16, flow_id: &str, req: RecordedRequest) -> Option<u64> {
            self.0.record_indexed(port, flow_id, req)
        }
        fn read(&self, port: u16) -> crate::imposter::JournalRead {
            self.0.read(port)
        }
        fn read_since(
            &self,
            port: u16,
            since: Option<u64>,
            keep: &dyn Fn(&RecordedRequest) -> bool,
        ) -> Option<crate::imposter::JournalReadSince> {
            self.0.read_since(port, since, keep).map(|mut r| {
                r.truncated = since.is_some();
                r
            })
        }
        fn clear(&self, port: u16) -> anyhow::Result<()> {
            self.0.clear(port)
        }
        fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
            self.0.retain(port, keep);
        }
        fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
            self.0.clear_flow(port, flow_id)
        }
        fn count(&self, port: u16) -> u64 {
            self.0.count(port)
        }
    }

    // AC12 (#603): `truncated` reaches the wire only when the backend reports a hole, and is
    // absent otherwise — SDKs treat its presence as "your baseline is stale, re-poll".
    #[tokio::test]
    async fn get_requests_flags_truncated_after_eviction() {
        let m = manager_with_journal(
            19756,
            Arc::new(TruncatingJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A")],
        )
        .await;

        let resp = handle_get_requests(19756, Some("since=1"), m.clone()).await;
        assert_eq!(
            header(&resp, "x-rift-truncated"),
            Some("true"),
            "the backend reported a hole; the client must be told"
        );
        assert_eq!(next_index(&resp), Some("2"), "cursor still advances");

        let resp = handle_get_requests(19756, None, m.clone()).await;
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "not truncated => header absent entirely, never 'false'"
        );

        let _ = m.delete_imposter(19756).await;
    }

    // AC12 (#603), end to end: drive the real 10k cap through the real recording path and
    // confirm a stale cursor is told it lost data. The double above pins the plumbing; this
    // pins that genuine retention pressure actually reaches the wire.
    #[tokio::test]
    async fn get_requests_truncated_by_real_cap_eviction() {
        let m = manager_with(19757, None, &[]).await;
        let imposter = m.get_imposter(19757).expect("imposter");
        for i in 0..(crate::imposter::MAX_RECORDED_REQUESTS + 5) {
            imposter.record_request(&rec_at(&format!("/{i}"), "A"));
        }

        // Indices 1..=5 were evicted; a cursor at 2 never received 3..=5.
        let resp = handle_get_requests(19757, Some("since=2"), m.clone()).await;
        assert_eq!(header(&resp, "x-rift-truncated"), Some("true"));
        assert_eq!(
            next_index(&resp),
            Some((crate::imposter::MAX_RECORDED_REQUESTS + 5).to_string()).as_deref()
        );

        // A cursor past the eviction watermark lost nothing.
        let resp = handle_get_requests(19757, Some("since=5"), m.clone()).await;
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "cursor at the watermark saw everything eviction removed"
        );

        let _ = m.delete_imposter(19757).await;
    }

    // A journal that reached only part of its storage (issue #314's degraded-read contract),
    // now on the cursor path: it returns fewer entries than it counted, but `next` still spans
    // the range it scanned.
    #[derive(Default)]
    struct DegradedJournal(crate::imposter::LocalJournal);
    impl crate::imposter::RequestJournal for DegradedJournal {
        fn note_request(&self, port: u16) {
            self.0.note_request(port);
        }
        fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
            self.0.record(port, flow_id, req);
        }
        fn record_indexed(&self, port: u16, flow_id: &str, req: RecordedRequest) -> Option<u64> {
            self.0.record_indexed(port, flow_id, req)
        }
        fn read(&self, port: u16) -> crate::imposter::JournalRead {
            self.0.read(port)
        }
        fn read_since(
            &self,
            port: u16,
            since: Option<u64>,
            keep: &dyn Fn(&RecordedRequest) -> bool,
        ) -> Option<crate::imposter::JournalReadSince> {
            self.0.read_since(port, since, keep).map(|mut r| {
                r.entries.truncate(1);
                r.complete = false;
                r
            })
        }
        fn clear(&self, port: u16) -> anyhow::Result<()> {
            self.0.clear(port)
        }
        fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
            self.0.retain(port, keep);
        }
        fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
            self.0.clear_flow(port, flow_id)
        }
        fn count(&self, port: u16) -> u64 {
            self.0.count(port)
        }
    }

    // A degraded read must not hand out a cursor: `next` spans entries the backend could not
    // serve, so advancing on it would skip them permanently and silently. Withholding the
    // header makes the client re-poll instead — partial data is served, but never at the cost
    // of losing the rest.
    #[tokio::test]
    async fn get_requests_withholds_cursor_on_degraded_read() {
        let m = manager_with_journal(
            19758,
            Arc::new(DegradedJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A"), rec_at("/c", "A")],
        )
        .await;

        for query in [None, Some("since=1")] {
            let resp = handle_get_requests(19758, query, m.clone()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(
                next_index(&resp).is_none(),
                "an incomplete read must not advertise a cursor to advance from"
            );
            assert!(header(&resp, "x-rift-truncated").is_none());
            assert_eq!(
                requests(resp).await.len(),
                1,
                "the partial results the backend did reach are still served"
            );
        }

        let _ = m.delete_imposter(19758).await;
    }
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::collections::HashMap;

    fn rec(method: &str, path: &str) -> RecordedRequest {
        RecordedRequest {
            request_from: "127.0.0.1:5000".to_string(),
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    async fn manager_with(port: u16, recorded: &[RecordedRequest]) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        let cfg = serde_json::json!({
            "port": port, "protocol": "http", "recordRequests": true, "stubs": []
        });
        let config = serde_json::from_value(cfg).expect("config");
        manager.create_imposter(config).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r);
        }
        manager
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn verify_counts_matched_and_total() {
        let m = manager_with(
            19751,
            &[rec("GET", "/a"), rec("POST", "/a"), rec("GET", "/b")],
        )
        .await;
        let body = br#"{"predicates":[{"equals":{"method":"GET"}}]}"#;
        let resp = verify_response(19751, body, &m, false);
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["matched"], 2);
        assert_eq!(json["total"], 3);
        assert!(json.get("requests").is_none());
        let _ = m.delete_imposter(19751).await;
    }

    #[tokio::test]
    async fn verify_include_requests_and_closest() {
        let m = manager_with(19752, &[rec("GET", "/a"), rec("DELETE", "/z")]).await;
        let body = br#"{"predicates":[{"equals":{"method":"GET"}}],"includeRequests":true,"includeClosest":true}"#;
        let resp = verify_response(19752, body, &m, false);
        let json = body_json(resp).await;
        assert_eq!(json["matched"], 1);
        assert_eq!(json["requests"].as_array().expect("requests").len(), 1);
        assert_eq!(json["closest"]["request"]["path"], "/z");
        let _ = m.delete_imposter(19752).await;
    }

    #[tokio::test]
    async fn verify_rejects_inject_predicate_without_allow_injection() {
        let m = manager_with(19753, &[rec("GET", "/a")]).await;
        let body = br#"{"predicates":[{"inject":"function(){return true;}"}]}"#;
        let resp = verify_response(19753, body, &m, false);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let json = body_json(resp).await;
        assert_eq!(json["errors"][0]["code"], "invalid injection");
        let _ = m.delete_imposter(19753).await;
    }

    #[tokio::test]
    async fn verify_bad_json_is_400() {
        let m = manager_with(19754, &[]).await;
        let resp = verify_response(19754, b"not json", &m, false);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let _ = m.delete_imposter(19754).await;
    }

    #[tokio::test]
    async fn verify_unknown_imposter_is_404() {
        let m = Arc::new(ImposterManager::new());
        let resp = verify_response(19755, br#"{"predicates":[]}"#, &m, false);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod replace_all_tests {
    use super::*;
    use http_body_util::BodyExt;

    const BASE: &str = "http://localhost:2525";

    async fn manager_with_http(port: u16) -> Arc<ImposterManager> {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(
            serde_json::json!({"port": port, "protocol": "http", "stubs": []}),
        )
        .expect("config");
        manager.create_imposter(config).await.expect("create");
        manager
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    // Issue #549: a partial apply failure must be surfaced (non-2xx with the failing port in the
    // body), the valid siblings must be applied, and a pre-existing imposter whose config is
    // unchanged must survive — never a silent 200 with a smaller set after a wholesale delete.
    #[tokio::test]
    async fn put_partial_failure_reports_and_preserves_survivors() {
        let manager = manager_with_http(19760).await;
        // 19760 unchanged; 19761 fails at apply time (garbage TLS material).
        let body = serde_json::json!({"imposters": [
            {"port": 19760, "protocol": "http", "stubs": []},
            {"port": 19761, "protocol": "https", "cert": "not a pem", "key": "not a pem", "stubs": []}
        ]})
        .to_string();
        let resp =
            replace_all_from_bytes(body.as_bytes(), BASE, Arc::clone(&manager), false, None).await;

        assert!(
            !resp.status().is_success(),
            "a partial failure must not report success, got {}",
            resp.status()
        );
        let json = body_json(resp).await;
        let failed: Vec<&str> = json["failed"]
            .as_array()
            .expect("failed array")
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert!(
            failed.iter().any(|f| f.starts_with("19761:")),
            "the failing port must be identified in the report: {json}"
        );
        assert_eq!(json["errors"][0]["code"], "500");
        assert!(
            manager.get_imposter(19760).is_ok(),
            "the unchanged imposter must survive the partial failure"
        );
        assert!(
            manager.get_imposter(19761).is_err(),
            "the failed imposter must not be running"
        );
        manager.delete_all().await;
    }

    // Mountebank contract pin: full success keeps the 200 + {"imposters": [...]} shape, the old
    // set is replaced by the new one.
    #[tokio::test]
    async fn put_success_replaces_set_with_imposter_list() {
        let manager = manager_with_http(19762).await;
        let body =
            serde_json::json!({"imposters": [{"port": 19763, "protocol": "http", "stubs": []}]})
                .to_string();
        let resp =
            replace_all_from_bytes(body.as_bytes(), BASE, Arc::clone(&manager), false, None).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let ports: Vec<u64> = json["imposters"]
            .as_array()
            .expect("imposters array")
            .iter()
            .filter_map(|i| i["port"].as_u64())
            .collect();
        assert_eq!(ports, vec![19763]);
        assert!(manager.get_imposter(19762).is_err(), "old set replaced");
        assert!(manager.get_imposter(19763).is_ok());
        manager.delete_all().await;
    }

    // The documented semantic delta of the #549 fix: an imposter whose config is unchanged is
    // reconciled as a no-op and keeps its runtime state — under the old wholesale delete+recreate
    // its recorded requests were wiped by every PUT.
    #[tokio::test]
    async fn put_unchanged_imposter_keeps_recorded_requests() {
        let manager = Arc::new(ImposterManager::new());
        let cfg = serde_json::json!({"port": 19766, "protocol": "http", "recordRequests": true, "stubs": []});
        let config = serde_json::from_value(cfg.clone()).expect("config");
        manager.create_imposter(config).await.expect("create");
        manager
            .get_imposter(19766)
            .expect("imposter")
            .record_request(&RecordedRequest {
                request_from: "127.0.0.1:5000".to_string(),
                method: "GET".to_string(),
                path: "/seen".to_string(),
                query: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                body: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            });

        let body = serde_json::json!({"imposters": [cfg]}).to_string();
        let resp =
            replace_all_from_bytes(body.as_bytes(), BASE, Arc::clone(&manager), false, None).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let recorded = manager
            .get_imposter(19766)
            .expect("still running")
            .get_recorded_requests();
        assert_eq!(
            recorded.len(),
            1,
            "an unchanged imposter must keep its runtime state across PUT"
        );
        manager.delete_all().await;
    }

    // Issue #549: a set that fails validation (duplicate port) must be rejected as a client error
    // with the running imposters completely untouched — under the old wholesale pre-delete they
    // were already gone by the time the creates started failing.
    #[tokio::test]
    async fn put_invalid_set_rejected_with_imposters_untouched() {
        let manager = manager_with_http(19764).await;
        let body = serde_json::json!({"imposters": [
            {"port": 19765, "protocol": "http", "stubs": []},
            {"port": 19765, "protocol": "http", "stubs": []}
        ]})
        .to_string();
        let resp =
            replace_all_from_bytes(body.as_bytes(), BASE, Arc::clone(&manager), false, None).await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a duplicate-port set is a client error"
        );
        assert!(
            manager.get_imposter(19764).is_ok(),
            "a rejected set must leave the running imposters untouched"
        );
        assert!(manager.get_imposter(19765).is_err());
        manager.delete_all().await;
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;
    use http_body_util::BodyExt;

    // Issue #558: the default list response carries per-imposter stubCount (and enabled) so
    // clients like the TUI can render the list from ONE request instead of an N+1 detail loop
    // whose per-imposter failures were silently dropped.
    #[tokio::test]
    async fn list_response_includes_stub_count_and_enabled() {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "port": 19770, "protocol": "http",
            "stubs": [
                {"predicates": [], "responses": [{"is": {"statusCode": 200}}]},
                {"predicates": [], "responses": [{"is": {"statusCode": 201}}]}
            ]
        }))
        .expect("config");
        manager.create_imposter(config).await.expect("create");

        let resp = handle_list(Arc::clone(&manager), None, "http://localhost:2525").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let entry = &json["imposters"][0];
        assert_eq!(
            entry["stubCount"], 2,
            "the list payload must carry the stub count: {entry}"
        );
        assert_eq!(entry["enabled"], true);

        // Boundary: a stubless imposter reports 0, not a missing field.
        let empty = serde_json::from_value(serde_json::json!({
            "port": 19771, "protocol": "http", "stubs": []
        }))
        .expect("config");
        manager.create_imposter(empty).await.expect("create");
        let resp = handle_list(Arc::clone(&manager), None, "http://localhost:2525").await;
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let stubless = json["imposters"]
            .as_array()
            .expect("array")
            .iter()
            .find(|i| i["port"] == 19771)
            .expect("stubless imposter listed");
        assert_eq!(stubless["stubCount"], 0);
        manager.delete_all().await;
    }

    // Issue #584: the list summary must carry recordRequests so the TUI's recording indicator
    // reflects real state instead of always deserializing a missing field to false.
    #[tokio::test]
    async fn list_response_includes_record_requests() {
        let manager = Arc::new(ImposterManager::new());
        let recording = serde_json::from_value(serde_json::json!({
            "port": 19772, "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        manager.create_imposter(recording).await.expect("create");
        let plain = serde_json::from_value(serde_json::json!({
            "port": 19773, "protocol": "http", "stubs": []
        }))
        .expect("config");
        manager.create_imposter(plain).await.expect("create");

        let resp = handle_list(Arc::clone(&manager), None, "http://localhost:2525").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let by_port = |port: u64| {
            json["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .find(|i| i["port"] == port)
                .unwrap_or_else(|| panic!("imposter {port} listed"))
                .clone()
        };
        assert_eq!(
            by_port(19772)["recordRequests"],
            true,
            "a recording imposter must report recordRequests: true in the list"
        );
        assert_eq!(
            by_port(19773)["recordRequests"],
            false,
            "a non-recording imposter must report recordRequests: false, not a missing field"
        );
        manager.delete_all().await;
    }
}
