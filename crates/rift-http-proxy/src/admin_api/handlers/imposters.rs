//! Imposter CRUD handlers.

use crate::admin_api::request_filter::{parse_match_clauses, parse_since, request_matches};
use crate::admin_api::types::{
    ADMIN_JSON_CONTENT_TYPE, ImposterDetail, ImposterListEntry, ImposterQueryParams,
    ImposterSummary, ListImpostersResponse, RiftImposterExtensions, StubWithLinks, TlsHeader,
    build_response_with_headers, collect_body, error_response, json_response, make_imposter_links,
    make_stub_links, serialize_or_500,
};
use crate::extensions::decorate::backend_error_response;
use crate::imposter::RecordedRequest;
use crate::imposter::{
    ApplyReport, Imposter, ImposterConfig, ImposterError, ImposterManager, ScriptBaseDir,
    ScriptFileAccess, Stub, StubResponse, VerifyOptions, resolve_scripts,
};
use crate::response::ErrorKind;
use crate::scripting::validate_stubs;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

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

/// What a script in an imposter written through the admin API may `require()` (issue #1611): only
/// modules under `--scripts-dir`, or nothing without one. The admin API is a network door, like
/// `--datadir` and an `https:` source.
pub(crate) fn admin_script_access(scripts_dir: &Option<Arc<PathBuf>>) -> ScriptFileAccess {
    ScriptFileAccess::network(scripts_dir.as_deref().map(PathBuf::as_path))
}

/// Refuse stubs whose `lookup` behaviors name a CSV file an admin-API imposter may never read
/// (issue #1617): a path outside `--scripts-dir`, or any path without one. Every admin write door
/// runs it with the access it gives the imposter, so the client hears at write time what the
/// imposter would otherwise refuse on every request. `None` when the stubs are allowed through.
pub(crate) fn reject_unreadable_lookups(
    stubs: &[Stub],
    scripts_dir: &Option<Arc<PathBuf>>,
) -> Option<Response<Full<Bytes>>> {
    crate::imposter::check_lookup_paths(stubs, &admin_script_access(scripts_dir))
        .err()
        .map(|e| error_response(StatusCode::BAD_REQUEST, &e))
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

/// The live imposter's `_rift.scriptEngine.defaultEngine`, for a stub door: the stub arrives without
/// its imposter's `_rift` block, so the default must come from the running config (issue #1159).
/// `"rhai"` when there is no such imposter — the door then answers `404` anyway.
pub(crate) fn imposter_default_script_engine(manager: &ImposterManager, port: u16) -> String {
    manager.get_imposter(port).map_or_else(
        |_| "rhai".to_owned(),
        |imposter| imposter.config.default_script_engine().to_owned(),
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
    if allow_injection || !crate::injection_gate::stubs_contain_script_surface(stubs) {
        return None;
    }
    Some(injection_disallowed_response())
}

/// The Mountebank-compatible `400 invalid injection` response returned when a request carries a
/// scripting surface (`inject`, decorate, …) but `--allowInjection` is off.
pub(crate) fn injection_disallowed_response() -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "errors": [{
            "code": ErrorKind::InvalidInjection.slug(),
            "type": ErrorKind::InvalidInjection.slug(),
            "message": "inject requires --allowInjection to be set. See \
                        http://www.mbtest.org/docs/api/injection for more information.",
        }]
    })
    .to_string();
    build_response_with_headers(
        StatusCode::BAD_REQUEST,
        [("Content-Type", ADMIN_JSON_CONTENT_TYPE)],
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
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
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

    if let Some(rejection) = reject_unreadable_lookups(&config.stubs, &scripts_dir) {
        return rejection;
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

    match manager
        .create_imposter_from(
            config,
            crate::imposter::Persistence::Datadir,
            admin_script_access(&scripts_dir),
        )
        .await
    {
        Ok(assigned_port) => {
            info!("Created imposter on port {}", assigned_port);
            // Return the full imposter details with 201 Created
            let response = handle_get(assigned_port, None, base_url, manager).await;
            let (parts, body) = response.into_parts();
            let mut new_parts = parts;
            new_parts.status = StatusCode::CREATED;
            // Mountebank answers a create with `Location: <imposter URL>` (the `self` link the
            // body also carries). A base URL that is not a valid header value (it echoes the
            // request's Host) simply gets no Location; the body still names the imposter.
            if let Ok(location) = hyper::header::HeaderValue::from_str(&format!(
                "{base_url}/imposters/{assigned_port}"
            )) {
                new_parts.headers.insert(hyper::header::LOCATION, location);
            }
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
    match imposter_list_value(&manager, query, base_url) {
        Ok(body) => json_response(StatusCode::OK, &body),
        Err(resp) => *resp,
    }
}

/// The `{"imposters": [...]}` body of `GET /imposters` (in the shape `query` selects): every
/// registered imposter, by ascending port. A body that cannot render is the `500` to serve instead.
fn imposter_list_value(
    manager: &ImposterManager,
    query: Option<&str>,
    base_url: &str,
) -> Result<serde_json::Value, Box<Response<Full<Bytes>>>> {
    imposters_value(&manager.list_imposters(), query, base_url)
}

/// The `{"imposters": [...]}` body for `imposters`, in the order given and the shape `query`
/// selects. Shared by `GET /imposters` (ascending) and the `PUT /imposters` success reply (request
/// order, issue #1563).
fn imposters_value(
    imposters: &[Arc<Imposter>],
    query: Option<&str>,
    base_url: &str,
) -> Result<serde_json::Value, Box<Response<Full<Bytes>>>> {
    let params = ImposterQueryParams::parse(query);
    let render = |v: Result<serde_json::Value, serde_json::Error>| {
        v.map_err(|e| {
            Box::new(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to serialize response body: {e}"),
            ))
        })
    };

    if params.replayable {
        let configs = imposters
            .iter()
            .map(|i| replayable_value(i, params.remove_proxies))
            .collect::<Result<Vec<_>, _>>();
        render(configs.map(|v| serde_json::json!({ "imposters": v })))
    } else if params.list {
        // Mountebank-compatible abbreviated listing: port, protocol, name, numberOfRequests, _links
        let entries: Vec<ImposterListEntry> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterListEntry {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    default_response: i.config.default_response.clone(),
                    number_of_requests: i.get_request_count(),
                    allow_cors: i.config.allow_cors.value().cloned(),
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();
        render(serde_json::to_value(&entries).map(|v| serde_json::json!({ "imposters": v })))
    } else {
        let summaries: Vec<ImposterSummary> = imposters
            .iter()
            .filter_map(|i| {
                i.config.port.map(|port| ImposterSummary {
                    protocol: i.config.protocol.clone(),
                    port,
                    name: i.config.name.clone(),
                    default_response: i.config.default_response.clone(),
                    number_of_requests: i.get_request_count(),
                    stub_count: i.stub_count(),
                    enabled: i.is_enabled(),
                    record_requests: i.config.record_requests,
                    allow_cors: i.config.allow_cors.value().cloned(),
                    links: make_imposter_links(base_url, port),
                })
            })
            .collect();
        render(serde_json::to_value(ListImpostersResponse {
            imposters: summaries,
        }))
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
    let reconcile = match reconcile_param(req.uri().query()) {
        Ok(reconcile) => reconcile,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    replace_all_from_bytes(
        &body,
        base_url,
        manager,
        allow_injection,
        scripts_dir,
        reconcile,
    )
    .await
}

/// `PUT /imposters?reconcile=true` (issue #1518): keep the runtime state of the imposters in the
/// body. Absent or `false` is Mountebank's semantics. Any other value is refused, so a typo cannot
/// quietly reset what the caller meant to keep.
fn reconcile_param(query: Option<&str>) -> Result<bool, String> {
    let mut reconcile = false;
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key == "reconcile" {
            reconcile = match value.as_ref() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(format!(
                        "Invalid query parameter reconcile={other:?}: expected true or false"
                    ));
                }
            };
        }
    }
    Ok(reconcile)
}

/// Reset the runtime state of every imposter in a `PUT /imposters` body that the reconcile kept
/// running (issue #1518): Mountebank recreates each one, so none keeps its cycles, journal or
/// state. A created or replaced port is already fresh, and a failed one is left as the report
/// describes it. A reset that fails joins `failed`.
fn restart_kept_imposters(manager: &ImposterManager, ports: &[u16], report: &mut ApplyReport) {
    for &port in ports {
        let fresh = report.created.contains(&port) || report.replaced.contains(&port);
        let failed = report.failed.iter().any(|(p, _)| *p == port);
        if fresh || failed {
            continue;
        }
        if let Err(e) = manager.restart_runtime_state(port) {
            report.failed.push((port, e));
        }
    }
}

/// Parse, validate, and apply a `PUT /imposters` batch. Split out from `handle_replace_all` so
/// the path is unit-testable without a `Request<Incoming>` (same seam as `verify_response`).
async fn replace_all_from_bytes(
    body: &[u8],
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
    reconcile: bool,
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

    for config in &batch.imposters {
        if let Some(rejection) = reject_unreadable_lookups(&config.stubs, &scripts_dir) {
            return rejection;
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

    // Each config's explicit port, in body order (`None` for port-less and port 0), collected
    // before the configs move into the reconcile: the success reply is rendered in this order
    // (issue #1563). Port-less configs are always fresh creates, so only explicit ports can need
    // a reset.
    let body_ports: Vec<Option<u16>> = batch
        .imposters
        .iter()
        .map(ImposterConfig::explicit_port)
        .collect();
    let explicit_ports: Vec<u16> = body_ports.iter().flatten().copied().collect();

    // Reconcile toward the new set instead of delete-all-then-recreate (issue #549): the old
    // wholesale pre-delete meant any create failure (e.g. a transiently-held port) silently lost
    // the previous imposters behind a 200. apply_config validates the whole set before touching
    // anything, only replaces what actually changed, and reports per-port failures instead of
    // log-and-continue. The imposters it kept running then get fresh runtime state in place, as
    // Mountebank's recreate gives them (issue #1518), unless `?reconcile=true` asks to keep it.
    let mut report = match manager
        .apply_config_from(batch.imposters, admin_script_access(&scripts_dir))
        .await
    {
        Ok(report) => report,
        // Set-level validation failure (bad protocol, duplicate port, duplicate stub id): a bad
        // request body, rejected before any imposter was touched.
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid imposter set (imposters unchanged): {e}"),
            );
        }
    };
    if !reconcile {
        restart_kept_imposters(&manager, &explicit_ports, &mut report);
    }

    // The report rides beside `imposters` (Mountebank clients read `imposters`; the port lists
    // mirror `POST /admin/reload`). Rendered after the reset, so it shows `numberOfRequests: 0`.
    // `imposters` lists the body's imposters in request order, as Mountebank does (issue #1563);
    // one deleted concurrently since the apply is simply absent, as it would be from a list.
    if report.failed.is_empty() {
        let answered: Vec<Arc<Imposter>> = request_order(&body_ports, &report.created)
            .into_iter()
            .filter_map(|port| manager.get_imposter(port).ok())
            .collect();
        return match imposters_value(&answered, None, base_url) {
            Ok(mut body) => {
                if let Some(obj) = body.as_object_mut() {
                    obj.extend(report.port_lists());
                }
                json_response(StatusCode::OK, &body)
            }
            Err(resp) => *resp,
        };
    }

    // Residual per-port failures (e.g. a bind race): the other ports are already reconciled, so
    // carry the full report — a partial failure is exactly when the client needs to know what did
    // apply (same contract as POST /admin/reload).
    let status = replace_failure_status(&report.failed);
    let failures: Vec<String> = report
        .failed
        .iter()
        .map(|(port, e)| match port {
            // Port 0 is the ApplyReport sentinel for auto-assigned (port-less) configs.
            0 => format!("auto-assign: {e}"),
            port => format!("{port}: {e}"),
        })
        .collect();
    let mut body = report.port_lists();
    body.insert(
        "errors".to_owned(),
        serde_json::json!([{
            "code": status.as_str(),
            "type": ErrorKind::for_status(status).slug(),
            "message": format!("Replace partially failed: {}", failures.join("; ")),
        }]),
    );
    body.insert("failed".to_owned(), serde_json::json!(failures));
    json_response(status, &serde_json::Value::Object(body))
}

/// `PUT /imposters` answers in request order, as Mountebank does (issue #1563): an explicit port is
/// itself; each port-less config takes the next auto-assigned port in `created`, which
/// `apply_config` lists after the explicit ports, in input order (issue #1112). A port-less slot
/// with no auto port left is skipped; that only happens when the apply failed, and the failure
/// reply carries no `imposters`.
fn request_order(body_ports: &[Option<u16>], created: &[u16]) -> Vec<u16> {
    let mut auto = created
        .iter()
        .copied()
        .filter(|port| !body_ports.contains(&Some(*port)));
    body_ports
        .iter()
        .filter_map(|slot| slot.or_else(|| auto.next()))
        .collect()
}

/// A reconcile whose every failure is the caller's address (taken, not local, not permitted,
/// unresolvable) is a bad request, as Mountebank answers it (issue #1500); any other failure in the
/// set makes it a server fault.
fn replace_failure_status(failed: &[(u16, ImposterError)]) -> StatusCode {
    let all_caller = failed
        .iter()
        .all(|(_, e)| matches!(e, ImposterError::PortInUse(_)) || e.is_caller_address_error());
    if all_caller {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// DELETE /imposters - Delete all imposters
pub async fn handle_delete_all(
    manager: Arc<ImposterManager>,
    _base_url: &str,
) -> Response<Full<Bytes>> {
    let report = manager.delete_all().await;
    if report.failed.is_empty() {
        return json_response(
            StatusCode::OK,
            &serde_json::json!({ "imposters": report.deleted }),
        );
    }
    // Issue #1124: an imposter whose datadir file could not be removed is still serving. The client
    // is told which, and which were deleted, in the Mountebank envelope plus the deleted list.
    let failures: Vec<String> = report
        .failed
        .iter()
        .map(|(port, e)| {
            error!(port, error = %e, "DELETE /imposters: imposter could not be deleted");
            format!("{port}: {e}")
        })
        .collect();
    let status = StatusCode::SERVICE_UNAVAILABLE;
    json_response(
        status,
        &serde_json::json!({
            "errors": [{
                "code": status.as_str(),
                "type": ErrorKind::for_status(status).slug(),
                "message": format!(
                    "Persistence error: {} imposter(s) could not be deleted and are still serving: {}",
                    report.failed.len(),
                    failures.join("; ")
                ),
            }],
            "imposters": report.deleted,
        }),
    )
}

/// The document `GET /imposters/:port` prints for an imposter. `DELETE /imposters/:port` answers
/// with the same one (issue #1399), so the two cannot drift apart.
fn imposter_detail(
    imposter: &Imposter,
    port: u16,
    base_url: &str,
    remove_proxies: bool,
) -> ImposterDetail {
    let mut stubs = imposter.get_stubs();

    if remove_proxies {
        stubs = filter_proxy_stubs(stubs);
    }

    // Cached stub-overlap analysis (issue #423): computed once in core on stub mutation,
    // not recomputed here on every read. Reflects the imposter's real stubs regardless of
    // the `removeProxies` view (the warnings are advisory).
    let warnings = imposter.stub_warnings();
    if !warnings.is_empty() {
        // Ignored config keys were logged once, when the imposter loaded (issue #1152);
        // re-logging them on every read would repeat them for as long as it exists.
        for warning in warnings.iter().filter(|w| {
            w.warning_type != crate::extensions::stub_analysis::WarningType::ConfigKeyIgnored
        }) {
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

    ImposterDetail {
        protocol: imposter.config.protocol.clone(),
        port: imposter.config.port.unwrap_or(port),
        name: imposter.config.name.clone(),
        default_response: imposter.config.default_response.clone(),
        number_of_requests: imposter.get_request_count(),
        enabled: imposter.is_enabled(),
        record_requests: imposter.config.record_requests,
        allow_cors: imposter.config.allow_cors.value().cloned(),
        tls: tls_header(&imposter.config),
        requests: imposter.get_recorded_requests(),
        stubs: stubs_with_links,
        links: make_imposter_links(base_url, port),
        rift: rift_extensions,
    }
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
        // The same document `GET /imposters?replayable=true` lists for this imposter (issue #1219).
        Ok(imposter) if params.replayable => {
            match replayable_value(&imposter, params.remove_proxies) {
                Ok(value) => json_response(StatusCode::OK, &value),
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("failed to serialize response body: {e}"),
                ),
            }
        }
        Ok(imposter) => json_response(
            StatusCode::OK,
            &imposter_detail(&imposter, port, base_url, params.remove_proxies),
        ),
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port - Delete a specific imposter
pub async fn handle_delete(
    port: u16,
    base_url: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.delete_imposter_returning(port).await {
        // The imposter as it was, as GET prints it, with the counters it ended on (issue #1399).
        Ok(imposter) => {
            info!("Deleted imposter on port {}", port);
            json_response(
                StatusCode::OK,
                &imposter_detail(&imposter, port, base_url, false),
            )
        }
        // Mountebank's teardown is idempotent: a port with no imposter answers `200` with `{}`
        // (issue #1371). The C-ABI `rift_delete_imposter` keeps its documented `-1`.
        Err(ImposterError::NotFound(_)) => json_response(StatusCode::OK, &serde_json::json!({})),
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
    // Through the manager, not the bare atomic: the toggle is config (issue
    // #817) — it persists to datadir and emits an event.
    match manager.set_imposter_enabled(port, enabled).await {
        Ok(()) => {
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
/// added to the config later (or carried in the flattened `extra` map, e.g. a leftover `redis` block
/// or `backend` key from a config written before they were removed) is excluded by default rather
/// than leaked.
fn expose_flow_state(fs: &rift_mock_core::imposter::RiftFlowStateConfig) -> serde_json::Value {
    let mut out = serde_json::Map::new();
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
    build_cursor_response(serialize_or_500(&entries), next, truncated)
}

/// Attach the cursor headers to an already-serialized body, or serve the serialization failure
/// verbatim.
///
/// Split from [`cursor_response`] so the ordering is enforced by types rather than by discipline:
/// the headers cannot be built before serialization has succeeded, because this function never
/// sees the payload. A 500 therefore can never advertise a cursor for a body that was not sent.
fn build_cursor_response(
    json: Result<String, Box<Response<Full<Bytes>>>>,
    next: u64,
    truncated: bool,
) -> Response<Full<Bytes>> {
    let json = match json {
        Ok(j) => j,
        Err(resp) => return *resp,
    };
    let mut headers = vec![
        (
            "Content-Type".to_string(),
            ADMIN_JSON_CONTENT_TYPE.to_string(),
        ),
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
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req, &manager.body_guard()).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    // An `inject` predicate evaluates synchronous Boa JavaScript that can run away (issue #476);
    // evaluate the whole verify on the blocking pool so it never head-of-line-blocks a tokio
    // worker. Like the live matcher (#476) there's no abort flag — Boa's loop-iteration cap (#327)
    // eventually frees the blocking thread. A non-inject verify pays only a cheap task hop, fine
    // since verify is not a per-request hot path.
    let door = admin_script_access(&scripts_dir);
    tokio::task::spawn_blocking(move || {
        verify_response(port, &body, &manager, allow_injection, &door)
    })
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
///
/// `door` is the admin door's script access: an `inject` predicate is the admin client's code,
/// so it never runs with more than that, whatever imposter it verifies (review of #1645).
fn verify_response(
    port: u16,
    body: &[u8],
    manager: &ImposterManager,
    allow_injection: bool,
    door: &ScriptFileAccess,
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
    if !allow_injection
        && opts
            .predicates
            .iter()
            .any(crate::injection_gate::predicate_has_inject)
    {
        return injection_disallowed_response();
    }
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.verify_from(&opts, door) {
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
    match manager.clear_proxy_responses(port).await {
        Ok(()) => handle_get(port, None, base_url, manager).await,
        Err(e) => e.into(),
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// The TLS metadata an https imposter's detail prints (Mountebank `createHeader` copies the https
/// server's metadata into every non-list form); `None` for any other protocol. `pub` so the FFI
/// detail (`rift_get_imposter`) prints the same keys as `GET /imposters/{port}`.
pub fn tls_header(config: &ImposterConfig) -> Option<TlsHeader> {
    (config.protocol == "https").then(|| TlsHeader {
        cert: config.cert.clone(),
        key: config.key.clone(),
        mutual_auth: config.mutual_auth,
        reject_unauthorized: config.reject_unauthorized,
        ca: config.ca.clone(),
        // Stored verbatim, printed upper-cased — Mountebank's `options.ciphers.toUpperCase()`.
        ciphers: config.ciphers.as_deref().map(str::to_uppercase),
    })
}

/// The replayable document one imposter prints (`?replayable=true`, `rift save`, the C-ABI
/// export): [`replayable_config`] serialized, with an https imposter's [`TlsHeader`] keys merged
/// on top — both client-auth flags, `ca` (`null` when unset) and `ciphers` upper-cased — as
/// Mountebank's replayable form carries its `httpsServer` metadata (issue #1576).
///
/// The keys are added here, in the printed view, and NOT by `ImposterConfig`'s `Serialize`: that
/// form is the `--datadir` file and rift-cluster's committed `PutImposter` bytes, which must stay
/// byte-identical for every existing input. The added keys re-import unchanged (`ca: null` decodes
/// as unset; a false flag is the default).
///
/// # Errors
/// A serialization failure of the config, which the callers answer as an encode error.
pub fn replayable_value(
    imposter: &Imposter,
    remove_proxies: bool,
) -> Result<serde_json::Value, serde_json::Error> {
    let config = replayable_config(imposter, remove_proxies);
    let mut value = serde_json::to_value(&config)?;
    if let Some(tls) = tls_header(&config) {
        // Both are structs, so both serialize to objects; `cert`/`key` carry the same values.
        match (value.as_object_mut(), serde_json::to_value(tls)?) {
            (Some(doc), serde_json::Value::Object(keys)) => doc.extend(keys),
            _ => {
                return Err(serde::ser::Error::custom(
                    "an https imposter's config or TLS metadata did not encode as an object",
                ));
            }
        }
    }
    Ok(value)
}

/// The replayable (`?replayable=true`) projection of one imposter: its config, carrying the stubs it
/// serves *now* — `config.stubs` is only what it was created with, so a stub added through the stub
/// routes or recorded by a proxy would otherwise be missing from the export (issue #1219). With
/// `remove_proxies`, proxy responses are stripped via [`filter_proxy_stubs`]. `pub` so the FFI
/// layer (issue #491) serves the SAME projection as both admin routes instead of re-implementing it.
///
/// Every `_proxyResponseTime` is dropped, as Mountebank's replayable form drops it (issue #1519).
pub fn replayable_config(imposter: &Imposter, remove_proxies: bool) -> ImposterConfig {
    let mut config = imposter.current_config();
    for stub in &mut config.stubs {
        stub.forget_proxy_response_times();
    }
    if remove_proxies {
        config.stubs = filter_proxy_stubs(config.stubs);
    }
    config
}

/// Filter out proxy responses from the stubs of `config` as given. Kept for embedders that call it;
/// note that an imposter's `config.stubs` are the stubs it was created with, so the admin routes and
/// the FFI export an imposter through [`replayable_config`], which reads the live ones (issue #1219).
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
                    responses: non_proxy_responses,
                    ..stub
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
/// A port no imposter in a test's fresh manager holds. Nothing binds it.
#[cfg(test)]
const UNUSED_PORT: u16 = 1;

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

    /// Issue #1093: `POST /imposters` admits a null behavior key without the flag, and a null
    /// next to a live `decorate` is still refused.
    #[test]
    fn null_behavior_keys_accepted_but_do_not_hide_a_live_script() {
        let with_behaviors = |behaviors: serde_json::Value| {
            cfg(json!({
                "protocol": "http",
                "stubs": [{
                    "responses": [{ "is": { "statusCode": 200, "body": "x" }, "_behaviors": behaviors }]
                }]
            }))
        };
        let null_only =
            with_behaviors(json!({ "wait": null, "decorate": null, "shellTransform": null }));
        assert!(reject_if_injection_disallowed(&null_only, false).is_none());

        let mixed = with_behaviors(json!({ "wait": null, "decorate": "function(config) { }" }));
        assert!(reject_if_injection_disallowed(&mixed, false).is_some());
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

    // ---- #610: the injection gate vs #608's second wait spelling -----------------------

    fn wait_cfg(wait: serde_json::Value) -> ImposterConfig {
        cfg(json!({
            "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200 },
                    "_behaviors": { "wait": wait }
                }]
            }]
        }))
    }

    // AC 610-1: pins today's B2 behaviour explicitly — a bare-string function wait is a script
    // surface. Previously only implied by the classifier's source.
    #[test]
    fn bare_string_function_wait_is_gated() {
        let config = wait_cfg(json!("function() { return 100; }"));
        assert!(
            reject_if_injection_disallowed(&config, false).is_some(),
            "a JS-function wait must require --allowInjection"
        );
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    // AC 610-2: THE BYPASS REGRESSION TEST. #608 makes `{"inject": ...}` parse and execute on Boa;
    // if the classifier still only matches `Function`, that config is admitted with injection off
    // and runs anyway. Both spellings are the same capability and must gate identically.
    #[test]
    fn object_form_inject_wait_is_gated_identically() {
        let config = wait_cfg(json!({ "inject": "function() { return 100; }" }));
        assert!(
            reject_if_injection_disallowed(&config, false).is_some(),
            "the object-form wait executes JS just like the bare string — it must not bypass the gate"
        );
        assert!(reject_if_injection_disallowed(&config, true).is_none());
    }

    // AC 610-3 / #1162: the gate fails closed on a `wait` it cannot prove is a plain delay, and the
    // parser now refuses such a block before the gate is consulted, so no `ImposterConfig` carrying
    // one can be built. The gate's own fail-closed table is in `injection_gate.rs`; this pins that
    // the parser is the first of the two layers.
    #[test]
    fn unrecognized_wait_shapes_do_not_reach_the_gate() {
        for wait in [
            json!({ "bogus": true }),
            json!({ "inject": 42 }),
            json!({ "min": 1 }),
            json!({ "min": "1", "max": "2" }),
            json!(true),
        ] {
            let parsed = serde_json::from_value::<ImposterConfig>(json!({
                "protocol": "http",
                "stubs": [{
                    "responses": [{ "is": { "statusCode": 200 }, "_behaviors": { "wait": wait } }]
                }]
            }));
            assert!(parsed.is_err(), "the parser must refuse wait {wait}");
        }
    }

    // Fail-closed must stay scoped to what the gate is actually for: a block with no script surface
    // is never an injection problem. (A malformed one such as `{"repeat": 2.0}` is refused by the
    // parser since #1162, before this gate; the gate's verdict on it is pinned in `injection_gate.rs`.)
    #[test]
    fn script_free_behaviors_are_not_an_injection_problem() {
        let script_free = [
            json!({ "repeat": 3 }),
            json!({ "wait": 100, "repeat": 2 }),
            json!({ "copy": { "from": "path", "into": "${P}",
                              "using": { "method": "regex", "selector": ".*" } } }),
        ];
        for behaviors in script_free {
            let config = cfg(json!({
                "protocol": "http",
                "stubs": [{
                    "responses": [{ "is": { "statusCode": 200 }, "_behaviors": behaviors }]
                }]
            }));
            assert!(
                reject_if_injection_disallowed(&config, false).is_none(),
                "{behaviors} carries no script surface — the injection gate must not reject it"
            );
        }
    }

    // AC 610-5: guard against over-closing — numeric waits carry no script surface and must stay
    // admissible with injection off.
    #[test]
    fn numeric_waits_are_not_script_surfaces() {
        for wait in [json!(500), json!({ "min": 1, "max": 9 })] {
            let config = wait_cfg(wait.clone());
            assert!(
                reject_if_injection_disallowed(&config, false).is_none(),
                "{wait} is not a scripting surface and must not require --allowInjection"
            );
        }
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
        assert_eq!(out.get("ttlSeconds").and_then(|v| v.as_i64()), Some(300));
        // A leftover `backend` key sits in `extra` and is not part of the allowlist (#1463).
        assert!(out.get("backend").is_none(), "backend is no longer exposed");
        let mut keys: Vec<&str> = out
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["flowIdSource", "ttlSeconds"],
            "only the allowlist is exposed"
        );
        // The credential string must not survive anywhere in the exposed value.
        assert!(!out.to_string().contains("secret"));
    }
}

#[cfg(test)]
mod requests_filter_tests {
    use super::*;
    use crate::imposter::ResponseMode;
    use http_body_util::BodyExt;
    use std::collections::HashMap;

    fn rec(space: &str, tenant: Option<&str>) -> RecordedRequest {
        let mut headers = HashMap::new();
        headers.insert("X-Mock-Space".to_string(), vec![space.to_string()]);
        if let Some(t) = tenant {
            headers.insert("X-Tenant".to_string(), vec![t.to_string()]);
        }
        RecordedRequest {
            body_omitted: false,
            mode: ResponseMode::Text,
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/p".to_string(),
            query: HashMap::new(),
            headers,
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        }
    }

    async fn manager_with(
        flow_id_source: Option<&str>,
        recorded: &[RecordedRequest],
    ) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let mut cfg = serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        });
        if let Some(src) = flow_id_source {
            cfg["_rift"] = serde_json::json!({
                "flowState": { "flowIdSource": src }
            });
        }
        let config = serde_json::from_value(cfg).expect("config");
        let port = manager.create_imposter(config).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r.clone());
        }
        (manager, port)
    }

    async fn requests(resp: Response<Full<Bytes>>) -> Vec<RecordedRequest> {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("decode requests")
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_returns_all_without_match() {
        let (m, port) = manager_with(None, &[rec("A", None), rec("B", None)]).await;
        let resp = handle_get_requests(port, None, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(requests(resp).await.len(), 2);
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_filters_by_header() {
        let (m, port) = manager_with(None, &[rec("A", None), rec("B", None), rec("A", None)]).await;
        let resp = handle_get_requests(port, Some("match=header:X-Mock-Space=A"), m.clone()).await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 2, "only the two X-Mock-Space=A requests");
        assert!(got.iter().all(|r| {
            r.headers
                .get("X-Mock-Space")
                .and_then(|v| v.first())
                .map(String::as_str)
                == Some("A")
        }));
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_no_match_is_empty_200() {
        let (m, port) = manager_with(None, &[rec("A", None)]).await;
        let resp =
            handle_get_requests(port, Some("match=header:X-Mock-Space=ZZZ"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(requests(resp).await.is_empty());
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_flow_id_header_source_parity() {
        let (m, port) = manager_with(
            Some("header:X-Mock-Space"),
            &[rec("A", None), rec("B", None)],
        )
        .await;
        let by_flow_id =
            requests(handle_get_requests(port, Some("match=flow_id=A"), m.clone()).await).await;
        let by_header = requests(
            handle_get_requests(port, Some("match=header:X-Mock-Space=A"), m.clone()).await,
        )
        .await;
        assert_eq!(by_flow_id.len(), 1);
        assert_eq!(by_flow_id[0].headers.get("X-Mock-Space").unwrap()[0], "A");
        assert_eq!(
            serde_json::to_value(&by_flow_id).unwrap(),
            serde_json::to_value(&by_header).unwrap(),
            "flow_id= and header: must return the identical slice when flow_id_source is that header"
        );
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_flow_id_imposter_port() {
        let (m, imposter_port) = manager_with(None, &[rec("A", None), rec("B", None)]).await;
        // default flow_id_source = imposter_port → every request shares flow_id = port
        let all = handle_get_requests(
            imposter_port,
            Some(&format!("match=flow_id={imposter_port}")),
            m.clone(),
        )
        .await;
        assert_eq!(requests(all).await.len(), 2);
        let none = handle_get_requests(imposter_port, Some("match=flow_id=9999"), m.clone()).await;
        assert!(requests(none).await.is_empty());
        let _ = m.delete_imposter(imposter_port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_multiple_match_anded() {
        let (m, port) = manager_with(
            None,
            &[
                rec("A", Some("t1")),
                rec("A", Some("t2")),
                rec("B", Some("t1")),
            ],
        )
        .await;
        let resp = handle_get_requests(
            port,
            Some("match=header:X-Mock-Space=A&match=header:X-Tenant=t1"),
            m.clone(),
        )
        .await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 1, "only the A+t1 request matches both clauses");
        let _ = m.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn get_requests_unknown_port_404() {
        let port = UNUSED_PORT;
        let m = Arc::new(ImposterManager::new());
        let resp = handle_get_requests(port, None, m).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_malformed_match_400() {
        let (m, port) = manager_with(None, &[rec("A", None)]).await;
        let resp = handle_get_requests(port, Some("match=query=a"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let _ = m.delete_imposter(port).await;
    }

    fn rec_mp(method: &str, path: &str, space: &str) -> RecordedRequest {
        let mut r = rec(space, None);
        r.method = method.to_string();
        r.path = path.to_string();
        r
    }

    // #700: `method=`/`path=` filter GET, compose with header/flow_id/since, and (like every other
    // clause) advance the cursor past rejected entries so a filtered tail never re-scans.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_filters_by_method_and_path() {
        let (m, port) = manager_with(
            Some("header:X-Mock-Space"),
            &[
                rec_mp("GET", "/orders", "alice"),
                rec_mp("POST", "/orders", "alice"),
                rec_mp("POST", "/orders", "bob"),
                rec_mp("POST", "/items", "alice"),
            ],
        )
        .await;

        // method= alone
        let got =
            requests(handle_get_requests(port, Some("match=method=POST"), m.clone()).await).await;
        assert_eq!(got.len(), 3, "three POSTs");

        // path= alone
        let got =
            requests(handle_get_requests(port, Some("match=path=/orders"), m.clone()).await).await;
        assert_eq!(got.len(), 3, "three /orders");

        // method + path + flow_id, AND-ed
        let resp = handle_get_requests(
            port,
            Some("match=flow_id=alice&match=method=POST&match=path=/orders"),
            m.clone(),
        )
        .await;
        let got = requests(resp).await;
        assert_eq!(got.len(), 1, "only alice's POST /orders");
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].path, "/orders");

        let _ = m.delete_imposter(port).await;
    }

    // #700 (AC4): a `since` window filtered by method/path still advances the cursor across every
    // scanned entry, even the ones the filter rejects.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_since_composes_with_method_path() {
        let (m, port) = manager_with(
            None,
            &[
                rec_mp("GET", "/a", "A"),
                rec_mp("POST", "/b", "A"),
                rec_mp("GET", "/c", "A"),
                rec_mp("POST", "/d", "A"),
            ],
        )
        .await;

        // since=2 cuts to entries 3 & 4 (/c GET, /d POST); the method filter rejects /c but the
        // cursor still advances past it to 4, so a filtered tail never re-scans the window.
        let resp = handle_get_requests(port, Some("since=2&match=method=POST"), m.clone()).await;
        assert_eq!(next_index(&resp), Some("4"), "cursor spans scanned entries");
        let got = requests(resp).await;
        assert_eq!(
            got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/d"],
            "cut at 2, then keep only POST"
        );

        let _ = m.delete_imposter(port).await;
    }

    // #700: `method=`/`path=` scope a targeted DELETE too — both surfaces share the parser.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_requests_targeted_clear_by_method() {
        let (m, port) = manager_with(
            None,
            &[
                rec_mp("GET", "/x", "A"),
                rec_mp("POST", "/x", "A"),
                rec_mp("GET", "/y", "A"),
            ],
        )
        .await;
        let base = "http://localhost:2525";
        handle_clear_requests(port, Some("match=method=POST"), base, m.clone()).await;
        let remaining = requests(handle_get_requests(port, None, m.clone()).await).await;
        assert_eq!(remaining.len(), 2, "only the POST was cleared");
        assert!(remaining.iter().all(|r| r.method == "GET"));
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_requests_targeted_clear_removes_only_slice() {
        let (m, port) = manager_with(None, &[rec("A", None), rec("B", None), rec("A", None)]).await;
        let base = "http://localhost:2525";
        handle_clear_requests(port, Some("match=header:X-Mock-Space=A"), base, m.clone()).await;
        let remaining = requests(handle_get_requests(port, None, m.clone()).await).await;
        assert_eq!(remaining.len(), 1, "only the B request survives");
        assert_eq!(remaining[0].headers.get("X-Mock-Space").unwrap()[0], "B");
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_requests_without_match_clears_all() {
        let (m, port) = manager_with(None, &[rec("A", None), rec("B", None)]).await;
        let base = "http://localhost:2525";
        handle_clear_requests(port, None, base, m.clone()).await;
        let remaining = requests(handle_get_requests(port, None, m.clone()).await).await;
        assert!(remaining.is_empty());
        let _ = m.delete_imposter(port).await;
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
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_requests_backend_clear_failure_maps_to_503() {
        let manager = Arc::new(
            ImposterManager::new().with_request_journal(Arc::new(FailingClearJournal::default())
                as Arc<dyn crate::imposter::RequestJournal>),
        );
        let cfg = serde_json::from_value(serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        let imposter_port = manager.create_imposter(cfg).await.expect("create");
        let base = "http://localhost:2525";
        let resp = handle_clear_requests(imposter_port, None, base, manager.clone()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        // Issue #801: the admin plane serves only the envelope on this door.
        assert!(
            json.get("error").is_none()
                && json.get("feature").is_none()
                && json.get("detail").is_none(),
            "top-level legacy keys were removed in 0.18.0 (#801), got: {json}"
        );
        assert_eq!(json["errors"][0]["code"], "503");
        assert_eq!(json["errors"][0]["type"], "backend unavailable");
        let _ = manager.delete_imposter(imposter_port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_requests_malformed_match_400() {
        let (m, port) = manager_with(None, &[rec("A", None)]).await;
        let base = "http://localhost:2525";
        let resp = handle_clear_requests(port, Some("match=query=a"), base, m.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // a rejected clear must leave the buffer untouched
        let remaining = requests(handle_get_requests(port, None, m.clone()).await).await;
        assert_eq!(
            remaining.len(),
            1,
            "malformed DELETE must not clear anything"
        );
        let _ = m.delete_imposter(port).await;
    }

    // ---- #603: savedRequests cursor ----------------------------------------------------

    fn header<'a>(resp: &'a Response<Full<Bytes>>, name: &str) -> Option<&'a str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    // AC3 (#606): a serialization failure must never be dressed with cursor headers describing a
    // payload that was never sent. Driving the real header-assembly path with a synthetic failure
    // — asserting the error is passed through untouched, rather than asserting error_response's
    // fixed shape, which would hold no matter what this function did.
    #[tokio::test]
    async fn build_cursor_response_passes_failures_through_without_cursor_headers() {
        let failed: Box<Response<Full<Bytes>>> = Box::new(crate::admin_api::types::error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "boom",
        ));
        let resp = build_cursor_response(Err(failed), 12, true);

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            header(&resp, "x-rift-next-index").is_none(),
            "a 500 must not advertise a cursor to advance from"
        );
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "nor a truncation flag, even though truncated=true was requested"
        );

        // The success path still attaches them, so the assertions above are about the failure
        // branch and not a helper that never sets headers at all.
        let ok = build_cursor_response(Ok("[]".to_string()), 12, true);
        assert_eq!(header(&ok, "x-rift-next-index"), Some("12"));
        assert_eq!(header(&ok, "x-rift-truncated"), Some("true"));
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
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_since_returns_newer_with_cursor_header() {
        let (m, port) = manager_with(
            None,
            &[rec_at("/a", "A"), rec_at("/b", "A"), rec_at("/c", "A")],
        )
        .await;

        let resp = handle_get_requests(port, Some("since=1"), m.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(next_index(&resp), Some("3"));
        let got = requests(resp).await;
        assert_eq!(
            got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/b", "/c"],
            "index 1 was already seen"
        );

        // Caught up: empty body, cursor unchanged.
        let resp = handle_get_requests(port, Some("since=3"), m.clone()).await;
        assert_eq!(next_index(&resp), Some("3"));
        assert!(requests(resp).await.is_empty());

        let _ = m.delete_imposter(port).await;
    }

    // AC8 (#603): `since` and `match` compose — cursor cut first, filter after — and a window
    // that matches nothing still advances the cursor.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_since_composes_with_match() {
        let (m, port) = manager_with(
            None,
            &[
                rec_at("/1", "A"),
                rec_at("/2", "B"),
                rec_at("/3", "A"),
                rec_at("/4", "B"),
            ],
        )
        .await;

        let resp =
            handle_get_requests(port, Some("since=1&match=header:X-Mock-Space=A"), m.clone()).await;
        assert_eq!(next_index(&resp), Some("4"), "cursor spans scanned entries");
        let got = requests(resp).await;
        assert_eq!(
            got.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/3"],
            "cut at 1, then keep only space A"
        );

        // A filtered tail whose window matches nothing must still advance, or it re-scans forever.
        let resp = handle_get_requests(
            port,
            Some("since=1&match=header:X-Mock-Space=ZZZ"),
            m.clone(),
        )
        .await;
        assert_eq!(next_index(&resp), Some("4"));
        assert!(requests(resp).await.is_empty());

        let _ = m.delete_imposter(port).await;
    }

    // AC9 (#603): a malformed cursor is rejected before any journal work.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_rejects_non_numeric_since() {
        let (m, port) = manager_with(None, &[rec_at("/a", "A")]).await;
        for bad in ["since=abc", "since=-1", "since=", "since=1.5"] {
            let resp = handle_get_requests(port, Some(bad), m.clone()).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "'{bad}' must be rejected"
            );
            assert!(next_index(&resp).is_none(), "no cursor header on a 400");
        }
        let _ = m.delete_imposter(port).await;
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
        journal: Arc<dyn crate::imposter::RequestJournal>,
        recorded: &[RecordedRequest],
    ) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new().with_request_journal(journal));
        let cfg = serde_json::from_value(serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        let port = manager.create_imposter(cfg).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r.clone());
        }
        (manager, port)
    }

    // AC10 (#603): against a backend without stable indices the endpoint behaves exactly as it
    // does today — full list, no cursor headers. That absence IS the SDK capability probe, so
    // it must hold even when the client asks for a cursor.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_omits_cursor_headers_without_support() {
        let (m, port) = manager_with_journal(
            Arc::new(CursorlessJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A")],
        )
        .await;

        for query in [None, Some("since=1")] {
            let resp = handle_get_requests(port, query, m.clone()).await;
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
        let _ = m.delete_imposter(port).await;
    }

    // AC11 (#603): the baseline poll (no `since`) is unchanged in body but carries the cursor
    // the SDK loop starts from — including on an empty journal.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_baseline_reports_next_index() {
        let (empty, port_a) = manager_with(None, &[]).await;
        let resp = handle_get_requests(port_a, None, empty.clone()).await;
        assert_eq!(next_index(&resp), Some("0"), "0 == 'seen nothing yet'");
        assert!(header(&resp, "x-rift-truncated").is_none());
        assert!(requests(resp).await.is_empty());
        let _ = empty.delete_imposter(port_a).await;

        let (m, port_b) = manager_with(None, &[rec_at("/a", "A"), rec_at("/b", "A")]).await;
        let resp = handle_get_requests(port_b, None, m.clone()).await;
        assert_eq!(next_index(&resp), Some("2"));
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "a baseline read is never truncated"
        );
        assert_eq!(requests(resp).await.len(), 2);
        let _ = m.delete_imposter(port_b).await;
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
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_flags_truncated_after_eviction() {
        let (m, port) = manager_with_journal(
            Arc::new(TruncatingJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A")],
        )
        .await;

        let resp = handle_get_requests(port, Some("since=1"), m.clone()).await;
        assert_eq!(
            header(&resp, "x-rift-truncated"),
            Some("true"),
            "the backend reported a hole; the client must be told"
        );
        assert_eq!(next_index(&resp), Some("2"), "cursor still advances");

        let resp = handle_get_requests(port, None, m.clone()).await;
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "not truncated => header absent entirely, never 'false'"
        );

        let _ = m.delete_imposter(port).await;
    }

    // AC12 (#603), end to end: drive real byte-budget eviction (#1441) through the real
    // recording path and confirm a stale cursor is told it lost data. The double above pins the
    // plumbing; this pins that genuine retention pressure actually reaches the wire.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_truncated_by_real_cap_eviction() {
        // Exactly KEPT of these fixed-cost entries fit the manager's journal budget.
        const KEPT: usize = 100;
        let budget = KEPT * crate::imposter::journal::entry_cost(&rec_at("/00000", "A"));
        let m = Arc::new(
            ImposterManager::new()
                .with_journal_max_bytes(std::num::NonZeroUsize::new(budget).expect("non-zero")),
        );
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        let imposter_port = m.create_imposter(config).await.expect("create");
        let imposter = m.get_imposter(imposter_port).expect("imposter");
        for i in 0..(KEPT + 5) {
            imposter.record_request(rec_at(&format!("/{i:05}"), "A"));
        }

        // Indices 1..=5 were evicted; a cursor at 2 never received 3..=5.
        let resp = handle_get_requests(imposter_port, Some("since=2"), m.clone()).await;
        assert_eq!(header(&resp, "x-rift-truncated"), Some("true"));
        assert_eq!(next_index(&resp), Some((KEPT + 5).to_string()).as_deref());

        // A cursor past the eviction watermark lost nothing.
        let resp = handle_get_requests(imposter_port, Some("since=5"), m.clone()).await;
        assert!(
            header(&resp, "x-rift-truncated").is_none(),
            "cursor at the watermark saw everything eviction removed"
        );

        let _ = m.delete_imposter(imposter_port).await;
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
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn get_requests_withholds_cursor_on_degraded_read() {
        let (m, port) = manager_with_journal(
            Arc::new(DegradedJournal::default()),
            &[rec_at("/a", "A"), rec_at("/b", "A"), rec_at("/c", "A")],
        )
        .await;

        for query in [None, Some("since=1")] {
            let resp = handle_get_requests(port, query, m.clone()).await;
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

        let _ = m.delete_imposter(port).await;
    }
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use crate::imposter::ResponseMode;
    use http_body_util::BodyExt;
    use std::collections::HashMap;

    fn rec(method: &str, path: &str) -> RecordedRequest {
        RecordedRequest {
            body_omitted: false,
            mode: ResponseMode::Text,
            request_from: "127.0.0.1:5000".to_string(),
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        }
    }

    async fn manager_with(recorded: &[RecordedRequest]) -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let cfg = serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        });
        let config = serde_json::from_value(cfg).expect("config");
        let port = manager.create_imposter(config).await.expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");
        for r in recorded {
            imposter.record_request(r.clone());
        }
        (manager, port)
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    // Review of #1645: a verify's `inject` predicate is written by the admin client, so it runs
    // with the admin door's access, never with the trusted access of a `--configfile` imposter it
    // verifies against.
    #[cfg(feature = "javascript")]
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn a_verify_inject_runs_with_the_admin_doors_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, r#"{"canary": 1645}"#).expect("canary");
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        let port = manager
            .create_imposter_from(
                config,
                crate::imposter::Persistence::Ephemeral,
                ScriptFileAccess::trusted_cwd(),
            )
            .await
            .expect("create");
        manager
            .get_imposter(port)
            .expect("imposter")
            .record_request(rec("GET", "/a"));
        let inject = format!(
            "config => {{ try {{ return require({:?}).canary === 1645; }} catch (e) {{ return false; }} }}",
            outside.to_str().expect("utf-8")
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "predicates": [{ "inject": inject }]
        }))
        .expect("body");
        let resp = verify_response(
            port,
            &body,
            &manager,
            true,
            &admin_script_access(&Some(Arc::new(dir.path().join("sd")))),
        );
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["matched"], 0,
            "the admin client's inject could read a file outside --scripts-dir: {json}"
        );
        let _ = manager.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn verify_counts_matched_and_total() {
        let (m, port) =
            manager_with(&[rec("GET", "/a"), rec("POST", "/a"), rec("GET", "/b")]).await;
        let body = br#"{"predicates":[{"equals":{"method":"GET"}}]}"#;
        let resp = verify_response(port, body, &m, false, &ScriptFileAccess::Refused);
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["matched"], 2);
        assert_eq!(json["total"], 3);
        assert!(json.get("requests").is_none());
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn verify_include_requests_and_closest() {
        let (m, port) = manager_with(&[rec("GET", "/a"), rec("DELETE", "/z")]).await;
        let body = br#"{"predicates":[{"equals":{"method":"GET"}}],"includeRequests":true,"includeClosest":true}"#;
        let resp = verify_response(port, body, &m, false, &ScriptFileAccess::Refused);
        let json = body_json(resp).await;
        assert_eq!(json["matched"], 1);
        assert_eq!(json["requests"].as_array().expect("requests").len(), 1);
        assert_eq!(json["closest"]["request"]["path"], "/z");
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn verify_rejects_inject_predicate_without_allow_injection() {
        let (m, port) = manager_with(&[rec("GET", "/a")]).await;
        let body = br#"{"predicates":[{"inject":"function(){return true;}"}]}"#;
        let resp = verify_response(port, body, &m, false, &ScriptFileAccess::Refused);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let json = body_json(resp).await;
        assert_eq!(json["errors"][0]["code"], "invalid injection");
        let _ = m.delete_imposter(port).await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn verify_bad_json_is_400() {
        let (m, port) = manager_with(&[]).await;
        let resp = verify_response(port, b"not json", &m, false, &ScriptFileAccess::Refused);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let _ = m.delete_imposter(port).await;
    }

    #[tokio::test]
    async fn verify_unknown_imposter_is_404() {
        let port = UNUSED_PORT;
        let m = Arc::new(ImposterManager::new());
        let resp = verify_response(
            port,
            br#"{"predicates":[]}"#,
            &m,
            false,
            &ScriptFileAccess::Refused,
        );
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod replace_all_tests {
    use super::*;
    use crate::imposter::ResponseMode;
    use http_body_util::BodyExt;

    const BASE: &str = "http://localhost:2525";

    async fn manager_with_http() -> (Arc<ImposterManager>, u16) {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
            .expect("config");
        let port = manager.create_imposter(config).await.expect("create");
        (manager, port)
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    // Issue #549: a partial apply failure must be surfaced (non-2xx with the failing port in the
    // body), the valid siblings must be applied, and a pre-existing imposter whose config is
    // unchanged must survive — never a silent 200 with a smaller set after a wholesale delete.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_partial_failure_reports_and_preserves_survivors() {
        let (manager, port_a) = manager_with_http().await;
        // <port_a> unchanged; the port-less https imposter fails at apply time (garbage TLS
        // material), so the report names it as the auto-assign entry.
        let body = serde_json::json!({"imposters": [
            {"port": port_a, "protocol": "http", "stubs": []},
            {"protocol": "https", "cert": "not a pem", "key": "not a pem", "stubs": []}
        ]})
        .to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

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
            failed.iter().any(|f| f.starts_with("auto-assign:")),
            "the failing port must be identified in the report: {json}"
        );
        assert_eq!(json["errors"][0]["code"], "500");
        assert_eq!(json["toggled"], serde_json::json!([]), "{json}");
        assert!(
            manager.get_imposter(port_a).is_ok(),
            "the unchanged imposter must survive the partial failure"
        );
        assert_eq!(
            manager.count(),
            1,
            "the failed imposter must not be running"
        );
        manager.delete_all().await;
    }

    // Mountebank contract pin: full success keeps the 200 + {"imposters": [...]} shape, the old
    // set is replaced by the new one.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_success_replaces_set_with_imposter_list() {
        let (manager, port_a) = manager_with_http().await;
        let body =
            serde_json::json!({"imposters": [{"protocol": "http", "stubs": []}]}).to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let ports: Vec<u64> = json["imposters"]
            .as_array()
            .expect("imposters array")
            .iter()
            .filter_map(|i| i["port"].as_u64())
            .collect();
        assert_eq!(ports.len(), 1, "{json}");
        for field in ["created", "replaced", "stubPatched", "toggled", "deleted"] {
            assert!(json[field].is_array(), "`{field}` must be an array: {json}");
        }
        // The new imposter is auto-assigned and may reuse the freed port, so the replacement is
        // pinned by the report rather than by `port_a` being gone.
        assert_eq!(json["created"], serde_json::json!(ports), "{json}");
        assert_eq!(
            json["deleted"],
            serde_json::json!([port_a]),
            "old set replaced: {json}"
        );
        let port_b = u16::try_from(ports[0]).expect("port");
        assert!(manager.get_imposter(port_b).is_ok());
        assert_eq!(manager.count(), 1);
        manager.delete_all().await;
    }

    // Issue #1563: a successful PUT answers in request order, as Mountebank does (`Promise.all`
    // over the body), with the port-less imposter at its own position — not by ascending port.
    // `GET /imposters` stays ascending.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_success_answers_imposters_in_request_order() {
        let manager = Arc::new(ImposterManager::new());
        let mut free = Vec::new();
        for _ in 0..2 {
            let config =
                serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
                    .expect("config");
            free.push(manager.create_imposter(config).await.expect("create"));
        }
        let (port_a, port_b) = (free[0].min(free[1]), free[0].max(free[1]));
        let body = serde_json::json!({"imposters": [
            {"protocol": "http", "port": port_b, "name": "b", "stubs": []},
            {"protocol": "http", "port": port_a, "name": "a", "stubs": []},
            {"protocol": "http", "name": "auto", "stubs": []},
        ]})
        .to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let created = ports_in(&json, "created");
        let auto = *created
            .iter()
            .find(|&&p| p != u64::from(port_a) && p != u64::from(port_b))
            .unwrap_or_else(|| panic!("the port-less imposter must be created: {json}"));
        let answered: Vec<(u64, &str)> = json["imposters"]
            .as_array()
            .expect("imposters array")
            .iter()
            .map(|i| {
                (
                    i["port"].as_u64().expect("port"),
                    i["name"].as_str().expect("name"),
                )
            })
            .collect();
        assert_eq!(
            answered,
            vec![
                (u64::from(port_b), "b"),
                (u64::from(port_a), "a"),
                (auto, "auto")
            ],
            "PUT answers in request order: {json}"
        );

        let list = body_json(handle_list(Arc::clone(&manager), None, BASE).await).await;
        let mut ascending = vec![u64::from(port_a), u64::from(port_b), auto];
        ascending.sort_unstable();
        let listed: Vec<u64> = list["imposters"]
            .as_array()
            .expect("imposters array")
            .iter()
            .filter_map(|i| i["port"].as_u64())
            .collect();
        assert_eq!(listed, ascending, "GET stays ascending: {list}");
        manager.delete_all().await;
    }

    // Issue #1563: request order is rebuilt from the body's explicit ports and `created`, which
    // lists explicit ports first (input order) and then auto-assigned ports (input order).
    #[test]
    fn request_order_places_each_auto_port_at_its_body_slot() {
        // Explicit-only, descending: the body order, not the port order.
        assert_eq!(request_order(&[Some(9), Some(3)], &[9, 3]), vec![9, 3]);
        // Mixed: each port-less slot takes the next auto port, in input order.
        assert_eq!(
            request_order(&[None, Some(62), None, Some(61)], &[62, 61, 1001, 1002]),
            vec![1001, 62, 1002, 61]
        );
        // An unchanged (kept or reset) explicit port is not in `created` but keeps its slot.
        assert_eq!(
            request_order(&[Some(7), None, Some(5)], &[5, 1001]),
            vec![7, 1001, 5]
        );
    }

    fn ports_in(json: &serde_json::Value, field: &str) -> Vec<u64> {
        json[field]
            .as_array()
            .unwrap_or_else(|| panic!("`{field}` must be an array: {json}"))
            .iter()
            .filter_map(|p| p.as_u64())
            .collect()
    }

    // Issue #1304: the apply report is computed on every PUT; success must carry it beside
    // `imposters` instead of discarding it.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_success_body_carries_the_apply_report() {
        let manager = Arc::new(ImposterManager::new());
        let imp = |body: &str| {
            serde_json::json!({
                "protocol": "http",
                "stubs": [{"responses": [{"is": {"statusCode": 200, "body": body}}]}]
            })
        };
        let mut ports = Vec::new();
        for body in ["a", "b", "d"] {
            let config = serde_json::from_value(imp(body)).expect("config");
            ports.push(manager.create_imposter(config).await.expect("create"));
        }
        let [port_a, port_b, port_c] = ports[..] else {
            unreachable!("three creates")
        };
        let at = |port: u16, body: &str| {
            let mut config = imp(body);
            config["port"] = serde_json::json!(port);
            config
        };
        // The fourth imposter is new and port-less: its auto-assigned port is read from `created`.
        let body = serde_json::json!({"imposters": [at(port_a, "a"), at(port_b, "b2"), imp("c")]})
            .to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let listed: Vec<u64> = json["imposters"]
            .as_array()
            .expect("imposters array")
            .iter()
            .filter_map(|i| i["port"].as_u64())
            .collect();
        assert_eq!(listed.len(), 3, "{json}");
        let created = ports_in(&json, "created");
        assert_eq!(created.len(), 1, "{json}");
        let port_d = created[0];
        for p in [u64::from(port_a), u64::from(port_b), port_d] {
            assert!(listed.contains(&p), "{json}");
        }
        assert_eq!(ports_in(&json, "deleted"), vec![u64::from(port_c)]);
        assert_eq!(ports_in(&json, "toggled"), Vec::<u64>::new());
        let changed: Vec<u64> = ["replaced", "stubPatched"]
            .iter()
            .flat_map(|f| ports_in(&json, f))
            .collect();
        assert!(changed.contains(&u64::from(port_b)), "{json}");
        assert!(
            !changed.contains(&u64::from(port_a)),
            "unchanged imposter in a list: {json}"
        );
        manager.delete_all().await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_success_reports_a_toggle() {
        let (manager, imposter_port) = manager_with_http().await;
        let body = serde_json::json!({"imposters": [
            {"port": imposter_port, "protocol": "http", "enabled": false, "stubs": []}
        ]})
        .to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            ports_in(&json, "toggled"),
            vec![u64::from(imposter_port)],
            "{json}"
        );
        for field in ["created", "replaced", "stubPatched", "deleted"] {
            assert!(ports_in(&json, field).is_empty(), "{field}: {json}");
        }
        manager.delete_all().await;
    }

    /// An unchanged imposter with one recorded request, and the `PUT /imposters` body that names it
    /// unchanged.
    async fn unchanged_with_one_request(manager: &ImposterManager) -> (u16, String) {
        let mut cfg = serde_json::json!({"protocol": "http", "recordRequests": true, "stubs": []});
        let config = serde_json::from_value(cfg.clone()).expect("config");
        let imposter_port = manager.create_imposter(config).await.expect("create");
        cfg["port"] = serde_json::json!(imposter_port);
        manager
            .get_imposter(imposter_port)
            .expect("imposter")
            .record_request(RecordedRequest {
                body_omitted: false,
                mode: ResponseMode::Text,
                request_from: "127.0.0.1:5000".to_string(),
                method: "GET".to_string(),
                path: "/seen".to_string(),
                query: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                body: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                match_outcome: None,
                status: None,
                latency_ms: None,
                node: None,
            });
        let body = serde_json::json!({"imposters": [cfg]}).to_string();
        (imposter_port, body)
    }

    // Issue #1518: Mountebank recreates every imposter in a PUT body, so an unchanged one starts
    // with an empty journal too. The reconcile (#549) still keeps it serving throughout.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_resets_unchanged_imposter_recorded_requests() {
        let manager = Arc::new(ImposterManager::new());
        let (imposter_port, body) = unchanged_with_one_request(&manager).await;

        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let imposter = manager.get_imposter(imposter_port).expect("still running");
        assert!(imposter.get_recorded_requests().is_empty());
        assert_eq!(imposter.get_request_count(), 0);
        manager.delete_all().await;
    }

    // `?reconcile=true` keeps #549's behaviour: an unchanged imposter keeps its runtime state.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_with_reconcile_true_keeps_recorded_requests() {
        let manager = Arc::new(ImposterManager::new());
        let (imposter_port, body) = unchanged_with_one_request(&manager).await;

        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            true,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let recorded = manager
            .get_imposter(imposter_port)
            .expect("still running")
            .get_recorded_requests();
        assert_eq!(
            recorded.len(),
            1,
            "an unchanged imposter keeps its runtime state across PUT ?reconcile=true"
        );
        manager.delete_all().await;
    }

    #[test]
    fn put_with_bad_reconcile_value_is_400() {
        assert_eq!(reconcile_param(None), Ok(false));
        assert_eq!(reconcile_param(Some("reconcile=true")), Ok(true));
        assert_eq!(reconcile_param(Some("other=1&reconcile=false")), Ok(false));
        let refused = reconcile_param(Some("reconcile=yes")).expect_err("a typo is refused");
        assert!(refused.contains("reconcile"), "{refused}");
        assert!(reconcile_param(Some("reconcile=")).is_err());
    }

    // Issue #549: a set that fails validation (duplicate port) must be rejected as a client error
    // with the running imposters completely untouched — under the old wholesale pre-delete they
    // were already gone by the time the creates started failing.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn put_invalid_set_rejected_with_imposters_untouched() {
        let (manager, port_a) = manager_with_http().await;
        let body = serde_json::json!({"imposters": [
            {"port": UNUSED_PORT, "protocol": "http", "stubs": []},
            {"port": UNUSED_PORT, "protocol": "http", "stubs": []}
        ]})
        .to_string();
        let resp = replace_all_from_bytes(
            body.as_bytes(),
            BASE,
            Arc::clone(&manager),
            false,
            None,
            false,
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a duplicate-port set is a client error"
        );
        assert!(
            manager.get_imposter(port_a).is_ok(),
            "a rejected set must leave the running imposters untouched"
        );
        assert_eq!(manager.count(), 1);
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
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn list_response_includes_stub_count_and_enabled() {
        let manager = Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
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
            "protocol": "http", "stubs": []
        }))
        .expect("config");
        let port_b = manager.create_imposter(empty).await.expect("create");
        let resp = handle_list(Arc::clone(&manager), None, "http://localhost:2525").await;
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let stubless = json["imposters"]
            .as_array()
            .expect("array")
            .iter()
            .find(|i| i["port"] == port_b)
            .expect("stubless imposter listed");
        assert_eq!(stubless["stubCount"], 0);
        manager.delete_all().await;
    }

    // Issue #584: the list summary must carry recordRequests so the TUI's recording indicator
    // reflects real state instead of always deserializing a missing field to false.
    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn list_response_includes_record_requests() {
        let manager = Arc::new(ImposterManager::new());
        let recording = serde_json::from_value(serde_json::json!({
            "protocol": "http", "recordRequests": true, "stubs": []
        }))
        .expect("config");
        let port_a = manager.create_imposter(recording).await.expect("create");
        let plain = serde_json::from_value(serde_json::json!({
            "protocol": "http", "stubs": []
        }))
        .expect("config");
        let port_b = manager.create_imposter(plain).await.expect("create");

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
            by_port(u64::from(port_a))["recordRequests"],
            true,
            "a recording imposter must report recordRequests: true in the list"
        );
        assert_eq!(
            by_port(u64::from(port_b))["recordRequests"],
            false,
            "a non-recording imposter must report recordRequests: false, not a missing field"
        );
        manager.delete_all().await;
    }
}

// Issue #1124: a delete whose datadir file cannot be removed is a 503, not a 200.
#[cfg(test)]
mod delete_persistence_tests {
    use super::*;
    use http_body_util::BodyExt;

    /// A datadir that has been replaced by a regular file after the imposter was persisted, so the
    /// unlink fails with ENOTDIR.
    async fn manager_with_unremovable_file(root: &std::path::Path) -> (Arc<ImposterManager>, u16) {
        let datadir = root.join("data");
        std::fs::create_dir(&datadir).expect("mkdir");
        let manager = Arc::new(ImposterManager::with_datadir(Some(datadir.clone())));
        let port = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
                    .expect("config"),
            )
            .await
            .expect("create");
        std::fs::rename(&datadir, root.join("moved")).expect("move datadir");
        std::fs::write(&datadir, "not a directory").expect("replace with a file");
        (manager, port)
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    /// Issue #1371: Mountebank's teardown is idempotent: deleting a port with no imposter answers
    /// `200` with `{}`, not `404`.
    #[tokio::test]
    async fn delete_of_a_missing_imposter_answers_200_with_an_empty_object() {
        let port = UNUSED_PORT;
        let manager = Arc::new(ImposterManager::new());
        let resp = handle_delete(port, "http://localhost:2525", manager).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(hyper::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(body_json(resp).await, serde_json::json!({}));
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_of_a_missing_imposter_touches_no_datadir_file() {
        let port_b = UNUSED_PORT;
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(ImposterManager::with_datadir(Some(
            dir.path().to_path_buf(),
        )));
        let port_a = manager
            .create_imposter(
                serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
                    .expect("config"),
            )
            .await
            .expect("create");
        assert!(dir.path().join(format!("{port_a}.json")).exists());

        let resp = handle_delete(port_b, "http://localhost:2525", Arc::clone(&manager)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, serde_json::json!({}));
        assert!(
            dir.path().join(format!("{port_a}.json")).exists(),
            "another imposter's file is untouched"
        );
        manager.delete_all().await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_one_answers_503_and_keeps_the_imposter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (manager, port) = manager_with_unremovable_file(dir.path()).await;

        let resp = handle_delete(port, "http://localhost:2525", Arc::clone(&manager)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        let message = body["errors"][0]["message"].as_str().unwrap_or_default();
        assert!(message.contains(&format!("{port}.json")), "got: {body}");
        assert!(manager.get_imposter(port).is_ok());

        std::fs::remove_file(dir.path().join("data")).expect("restore");
        std::fs::create_dir(dir.path().join("data")).expect("restore");
        manager.delete_all().await;
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn delete_all_answers_503_naming_the_port_and_lists_what_was_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (manager, port_a) = manager_with_unremovable_file(dir.path()).await;
        let port_b = manager
            .create_imposter_as(
                serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
                    .expect("config"),
                crate::imposter::Persistence::Ephemeral,
            )
            .await
            .expect("create ephemeral");

        let resp = handle_delete_all(Arc::clone(&manager), "http://localhost:2525").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        let message = body["errors"][0]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&format!("{port_a}")) && message.contains(&format!("{port_a}.json")),
            "got: {body}"
        );
        assert_eq!(body["imposters"][0]["port"], port_b, "got: {body}");
        assert_eq!(body["imposters"].as_array().map(Vec::len), Some(1));
        assert!(manager.get_imposter(port_a).is_ok());

        std::fs::remove_file(dir.path().join("data")).expect("restore");
        std::fs::create_dir(dir.path().join("data")).expect("restore");
        manager.delete_all().await;
    }

    fn bind_failure(source: anyhow::Error) -> (u16, ImposterError) {
        (4545, ImposterError::BindError(4545, source))
    }

    // Issue #1500: a reconcile is a bad request only when every failure is the caller's address.
    #[test]
    fn replace_status_is_400_only_when_every_failure_is_the_callers_address() {
        let taken = || {
            bind_failure(anyhow::Error::new(std::io::Error::from(
                std::io::ErrorKind::AddrInUse,
            )))
        };
        let fault = || bind_failure(anyhow::anyhow!("no socket address"));

        assert_eq!(replace_failure_status(&[taken()]), StatusCode::BAD_REQUEST);
        assert_eq!(
            replace_failure_status(&[taken(), (4546, ImposterError::PortInUse(4546))]),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            replace_failure_status(&[taken(), fault()]),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            replace_failure_status(&[fault()]),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

// Issue #1576: every https view — create answer, detail and `?replayable=true` — prints the keys
// Mountebank's `httpsServer` metadata carries: both client-auth flags, `ca` (`null` when unset, in
// the spelling it was posted in) and `ciphers` (upper-cased) when set.
#[cfg(test)]
mod https_view_tests {
    use super::*;
    use http_body_util::BodyExt;

    const BASE: &str = "http://localhost:2525";

    fn config(body: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(body).expect("config")
    }

    fn header_json(body: serde_json::Value) -> serde_json::Value {
        serde_json::to_value(tls_header(&config(body)).expect("https")).expect("serialize")
    }

    async fn body_json(resp: Response<Full<Bytes>>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    #[test]
    fn tls_header_prints_ca_null_when_unset() {
        let json = header_json(serde_json::json!({"protocol": "https"}));
        assert_eq!(json.get("ca"), Some(&serde_json::Value::Null), "{json}");
        assert_eq!(json["mutualAuth"], false);
        assert_eq!(json["rejectUnauthorized"], false);
        assert!(json.get("ciphers").is_none(), "{json}");
    }

    #[test]
    fn tls_header_prints_ca_in_the_spelling_it_was_posted_in() {
        for ca in [
            serde_json::json!(["A"]),
            serde_json::json!("A"),
            serde_json::json!(["A", "B"]),
        ] {
            let json = header_json(serde_json::json!({
                "protocol": "https", "mutualAuth": true, "rejectUnauthorized": true, "ca": ca
            }));
            assert_eq!(json["ca"], ca);
        }
    }

    #[test]
    fn tls_header_prints_ciphers_upper_cased() {
        let json = header_json(
            serde_json::json!({"protocol": "https", "ciphers": "ecdhe-rsa-aes128-gcm-sha256"}),
        );
        assert_eq!(json["ciphers"], "ECDHE-RSA-AES128-GCM-SHA256");
    }

    #[serial_test::serial(auto_assigned_port)]
    #[tokio::test]
    async fn replayable_https_views_carry_the_client_auth_flags_and_ca_null() {
        let manager = Arc::new(ImposterManager::new());
        let https = manager
            .create_imposter(config(
                serde_json::json!({"protocol": "https", "ciphers": "aes128-sha", "stubs": []}),
            ))
            .await
            .expect("create https");
        let http = manager
            .create_imposter(config(
                serde_json::json!({"protocol": "http", "ciphers": "aes128-sha", "stubs": []}),
            ))
            .await
            .expect("create http");

        let one =
            body_json(handle_get(https, Some("replayable=true"), BASE, Arc::clone(&manager)).await)
                .await;
        let list =
            body_json(handle_list(Arc::clone(&manager), Some("replayable=true"), BASE).await).await;
        let listed = |port: u16| {
            list["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .find(|i| i["port"] == port)
                .cloned()
                .unwrap_or_else(|| panic!("{port} missing: {list}"))
        };
        for view in [one, listed(https)] {
            assert_eq!(view["mutualAuth"], false, "{view}");
            assert_eq!(view["rejectUnauthorized"], false, "{view}");
            assert_eq!(view.get("ca"), Some(&serde_json::Value::Null), "{view}");
            assert_eq!(view["ciphers"], "AES128-SHA", "{view}");
        }
        let plain = listed(http);
        for k in ["mutualAuth", "rejectUnauthorized", "ca", "ciphers"] {
            assert!(
                plain.get(k).is_none(),
                "http replayable has no `{k}`: {plain}"
            );
        }

        // The stored form is untouched: the config the imposter keeps has no flags and no `ca`,
        // and `ciphers` stays as posted.
        let stored =
            serde_json::to_value(&manager.get_imposter(https).expect("imposter").config).unwrap();
        for k in ["mutualAuth", "rejectUnauthorized", "ca"] {
            assert!(
                stored.get(k).is_none(),
                "stored form has no `{k}`: {stored}"
            );
        }
        assert_eq!(stored["ciphers"], "aes128-sha");

        // The replayable document re-imports.
        let again: ImposterConfig =
            serde_json::from_value(listed(https)).expect("replayable decodes");
        manager.delete_all().await;
        manager.create_imposter(again).await.expect("re-import");
        manager.delete_all().await;
    }
}
