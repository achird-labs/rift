//! Scenario FSM + flow-state admin handlers (issue #190).
//!
//! Scenario state and arbitrary flow-state both live in the imposter's `FlowStore`,
//! partitioned by `flow_id`. When a `flowId` is not supplied, the imposter's default
//! flow (`resolve_flow_id` with no headers ⇒ the `imposter_port` flow) is used.

use crate::admin_api::handlers::imposters::{
    admin_script_base, imposter_script_registry, reject_stubs_if_injection_disallowed,
};
use crate::admin_api::types::{collect_body, error_response, json_response};
use crate::extensions::decorate::backend_error_response;
use crate::imposter::{Imposter, ImposterManager, Stub, resolve_stub_scripts};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn default_flow_id(imposter: &Imposter) -> String {
    imposter.resolve_flow_id(&HashMap::new())
}

/// Collect and JSON-parse a request body, returning a `400` response on failure.
async fn parse_json_body(
    req: Request<Incoming>,
) -> Result<serde_json::Value, Response<Full<Bytes>>> {
    let body = collect_body(req)
        .await
        .map_err(|e| error_response(e.status_code(), &e.to_string()))?;
    serde_json::from_slice(&body)
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}")))
}

/// Extract `flowId=` from a query string.
fn flow_id_from_query(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "flowId").then(|| {
            // Domain-optional decode: an undecodable value passes through raw, the repo's
            // convention for percent-decoding query values (issue #611).
            urlencoding::decode(v)
                .map(|d| d.into_owned())
                .unwrap_or_else(|_| v.to_string())
        })
    })
}

/// GET /imposters/:port/scenarios[?flowId=] → `{flowId, scenarios:[{name,state}]}`
pub async fn handle_list_scenarios(
    port: u16,
    query: Option<&str>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = flow_id_from_query(query).unwrap_or_else(|| default_flow_id(&imposter));
            let mut scenarios = Vec::new();
            for name in imposter.scenario_names() {
                match imposter.scenario_state(&flow_id, &name) {
                    Ok(state) => {
                        scenarios.push(serde_json::json!({ "name": name, "state": state }))
                    }
                    Err(e) => return backend_error_response(&e),
                }
            }
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "scenarios": scenarios }),
            )
        }
        Err(e) => e.into(),
    }
}

/// PUT /imposters/:port/scenarios/:name/state — body `{"state":"…","flowId":"…"?}`
pub async fn handle_set_scenario_state(
    port: u16,
    name: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(state) = payload.get("state").and_then(|v| v.as_str()) else {
        return error_response(StatusCode::BAD_REQUEST, "missing required field: state");
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = payload
                .get("flowId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| default_flow_id(&imposter));
            match imposter.set_scenario_state(&flow_id, name, state) {
                Ok(()) => json_response(
                    StatusCode::OK,
                    &serde_json::json!({ "flowId": flow_id, "name": name, "state": state }),
                ),
                Err(e) => backend_error_response(&e),
            }
        }
        Err(e) => e.into(),
    }
}

/// POST /imposters/:port/scenarios/reset — body `{"flowId":"…"?}` (resets ONLY that flow's slice).
pub async fn handle_reset_scenarios(
    port: u16,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(e.status_code(), &e.to_string()),
    };
    let flow_id_opt = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => v.get("flowId").and_then(|v| v.as_str()).map(String::from),
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}"));
            }
        }
    };
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let flow_id = flow_id_opt.unwrap_or_else(|| default_flow_id(&imposter));
            for name in imposter.scenario_names() {
                if let Err(e) = imposter.delete_scenario_state(&flow_id, &name) {
                    return backend_error_response(&e);
                }
            }
            json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "reset": true }),
            )
        }
        Err(e) => e.into(),
    }
}

/// GET /admin/imposters/:port/flow-state/:flow_id/:key → `{flowId,key,value}` | 404
pub async fn handle_get_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_get(flow_id, key) {
            Ok(Some(value)) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "value": value }),
            ),
            Ok(None) => error_response(StatusCode::NOT_FOUND, "flow-state key not found"),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// PUT /admin/imposters/:port/flow-state/:flow_id/:key — body `{"value": …}`
pub async fn handle_put_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(value) = payload.get("value") else {
        return error_response(StatusCode::BAD_REQUEST, "missing required field: value");
    };
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_set(flow_id, key, value.clone()) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "value": value }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// DELETE /admin/imposters/:port/flow-state/:flow_id — clear every key in a flow (issue #530).
/// Idempotent: clearing an absent/empty flow still returns 200. 404 only when the imposter/port
/// does not exist.
pub async fn handle_clear_flow_state(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_clear(flow_id) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "cleared": true }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

/// DELETE /admin/imposters/:port/flow-state/:flow_id/:key
pub async fn handle_delete_flow_state(
    port: u16,
    flow_id: &str,
    key: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => match imposter.flow_delete(flow_id, key) {
            Ok(()) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "flowId": flow_id, "key": key, "deleted": true }),
            ),
            Err(e) => backend_error_response(&e),
        },
        Err(e) => e.into(),
    }
}

// ── Correlated-isolation "space" endpoints (issue #223) ─────────────────────────

/// POST /imposters/:port/spaces/:flowId/stubs — register a stub scoped to that space.
pub async fn handle_add_space_stub(
    port: u16,
    flow_id: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    let payload = match parse_json_body(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut stub: Stub = match serde_json::from_value(payload) {
        Ok(s) => s,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("Invalid stub: {e}")),
    };
    // Gate any scripting surface behind --allowInjection before mutating state (B3, issue #355).
    if let Some(rejection) =
        reject_stubs_if_injection_disallowed(std::slice::from_ref(&stub), allow_injection)
    {
        return rejection;
    }
    // Resolve `_rift.script` `file:`/`ref:` sources before persisting (issue #356 B1): escape /
    // unknown-ref / unconfigured `file:` → 400, nothing unresolved is ever stored.
    {
        let registry = imposter_script_registry(&manager, port);
        let base = admin_script_base(&scripts_dir);
        if let Err(e) = resolve_stub_scripts(std::slice::from_mut(&mut stub), &registry, &base) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Script resolution failed: {e}"),
            );
        }
    }
    // The path `:flowId` is the source of truth for the scope; ignore any `space` in the body.
    stub.space = Some(flow_id.to_string());
    match manager.add_stub(port, stub, None).await {
        Ok(()) => match manager.get_imposter(port) {
            Ok(imposter) => json_response(
                StatusCode::CREATED,
                &serde_json::json!({ "space": flow_id, "stubs": imposter.space_stubs(flow_id) }),
            ),
            Err(e) => e.into(),
        },
        Err(e) => e.into(),
    }
}

/// GET /imposters/:port/spaces/:flowId/stubs — list a space's scoped stubs.
pub async fn handle_list_space_stubs(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "space": flow_id, "stubs": imposter.space_stubs(flow_id) }),
        ),
        Err(e) => e.into(),
    }
}

/// GET /imposters/:port/spaces/:flowId — inspect a space (stubs + scenario states + request count).
pub async fn handle_get_space(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.get_imposter(port) {
        Ok(imposter) => {
            let mut scenarios = Vec::new();
            for name in imposter.scenario_names() {
                match imposter.scenario_state(flow_id, &name) {
                    Ok(state) => {
                        scenarios.push(serde_json::json!({ "name": name, "state": state }))
                    }
                    Err(e) => return backend_error_response(&e),
                }
            }
            let number_of_requests = imposter
                .get_recorded_requests()
                .iter()
                .filter(|r| imposter.resolve_flow_id_recorded(&r.headers) == flow_id)
                .count();
            json_response(
                StatusCode::OK,
                &serde_json::json!({
                    "space": flow_id,
                    "stubs": imposter.space_stubs(flow_id),
                    "scenarios": scenarios,
                    "numberOfRequests": number_of_requests
                }),
            )
        }
        Err(e) => e.into(),
    }
}

/// DELETE /imposters/:port/spaces/:flowId — one-call per-space teardown
/// (scoped stubs + recorded requests + scenario state), never a global reset.
pub async fn handle_teardown_space(
    port: u16,
    flow_id: &str,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    match manager.teardown_space(port, flow_id).await {
        Ok(()) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "space": flow_id, "tornDown": true }),
        ),
        Err(e) => e.into(),
    }
}
