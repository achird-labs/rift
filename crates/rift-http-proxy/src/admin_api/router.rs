//! Route dispatch logic for the Admin API.
//!
//! This module provides routing

use crate::admin_api::handlers::{imposters, intercept, scenarios, stubs, system};
use crate::admin_api::types::{error_response, get_base_url, not_found};
use crate::config_loader::ConfigSource;
use crate::imposter::ImposterManager;
use crate::intercept_rules::InterceptState;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

/// Parsed route for imposter-specific endpoints
enum ImposterRoute {
    /// GET/DELETE /imposters/:port
    Root,
    /// POST/PUT/GET /imposters/:port/stubs
    Stubs,
    /// GET/PUT/DELETE /imposters/:port/stubs/:index
    StubByIndex(usize),
    /// GET/PUT/DELETE /imposters/:port/stubs/by-id/:id (issue #202)
    StubById(String),
    /// DELETE /imposters/:port/savedRequests
    SavedRequests,
    /// POST /imposters/:port/verify (issue #494)
    Verify,
    /// DELETE /imposters/:port/savedProxyResponses
    SavedProxyResponses,
    /// POST /imposters/:port/enable
    Enable,
    /// POST /imposters/:port/disable
    Disable,
    /// GET /imposters/:port/scenarios
    Scenarios,
    /// PUT /imposters/:port/scenarios/:name/state
    ScenarioState(String),
    /// POST /imposters/:port/scenarios/reset
    ScenariosReset,
    /// GET/DELETE /imposters/:port/spaces/:flowId
    Space(String),
    /// POST/GET /imposters/:port/spaces/:flowId/stubs
    SpaceStubs(String),
}

impl ImposterRoute {
    /// Parse route from path segments after `/imposters/:port`
    fn parse(segments: &[&str]) -> Option<Self> {
        match segments {
            [] => Some(ImposterRoute::Root),
            ["stubs"] => Some(ImposterRoute::Stubs),
            ["stubs", "by-id", id] => Some(ImposterRoute::StubById((*id).to_string())),
            ["stubs", index_str] => index_str.parse().ok().map(ImposterRoute::StubByIndex),
            ["savedRequests"] | ["requests"] => Some(ImposterRoute::SavedRequests),
            ["verify"] => Some(ImposterRoute::Verify),
            ["savedProxyResponses"] => Some(ImposterRoute::SavedProxyResponses),
            ["enable"] => Some(ImposterRoute::Enable),
            ["disable"] => Some(ImposterRoute::Disable),
            ["scenarios"] => Some(ImposterRoute::Scenarios),
            ["scenarios", "reset"] => Some(ImposterRoute::ScenariosReset),
            ["scenarios", name, "state"] => Some(ImposterRoute::ScenarioState((*name).to_string())),
            ["spaces", flow_id] => Some(ImposterRoute::Space((*flow_id).to_string())),
            ["spaces", flow_id, "stubs"] => Some(ImposterRoute::SpaceStubs((*flow_id).to_string())),
            _ => None,
        }
    }
}

/// Main request router
#[allow(clippy::too_many_arguments)]
pub async fn route_request(
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    config_source: Option<Arc<ConfigSource>>,
    allow_injection: bool,
    intercept: Option<Arc<InterceptState>>,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|s| s.to_string());
    let base_url = get_base_url(&req);

    debug!("Admin API: {} {}", method, path);

    // Intercept control-plane routes (epic #394 slice 4): only reachable when an intercept
    // listener is actually running (i.e. the server was built `with_intercept(...)`). Nothing
    // else in `route_by_path` handles `/intercept`, so an unmatched sub-path or a missing
    // intercept state both fall through to the ordinary 404.
    if path == "/intercept" || path.starts_with("/intercept/") {
        let resp = match intercept.as_ref() {
            Some(state) => intercept::route(&method, &path, query.as_deref(), req, state)
                .await
                .unwrap_or_else(not_found),
            None => not_found(),
        };
        return Ok(resp);
    }

    let response = route_by_path(
        &method,
        &path,
        query.as_deref(),
        req,
        &base_url,
        manager,
        config_source,
        allow_injection,
        scripts_dir,
    )
    .await;
    Ok(response)
}

/// Dispatch a `/__rift/:port/<path>` gateway request to the imposter on `:port` (issue #212),
/// rewriting the URI to the imposter-relative `/<path>` (+ query) so its predicates and handler
/// behave exactly as if the request had arrived on the imposter's own port. The dispatch core
/// is the library-public `gateway::dispatch_to_port` (issue #317).
async fn handle_gateway(
    rest: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let (port_str, sub_path) = match rest.split_once('/') {
        Some((port, sub)) => (port, format!("/{sub}")),
        None => (rest, "/".to_string()),
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid gateway target '{port_str}' (expected /__rift/<port>/<path>)"),
        );
    };
    // Check existence before the URI rewrite so a missing imposter stays a 404 even if the
    // rewritten URI would be rejected — the pre-#317 response precedence. dispatch_to_port
    // re-checks as its own defensive 404 for other callers.
    if manager.get_imposter(port).is_err() {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("no imposter on port {port}"),
        );
    }

    let target = match query {
        Some(q) => format!("{sub_path}?{q}"),
        None => sub_path,
    };
    let (mut parts, body) = req.into_parts();
    parts.uri = match target.parse() {
        Ok(uri) => uri,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid gateway path"),
    };

    crate::gateway::dispatch_to_port(&manager, port, Request::from_parts(parts, body)).await
}

/// Route based on path
#[allow(clippy::too_many_arguments)]
async fn route_by_path(
    method: &Method,
    path: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    config_source: Option<Arc<ConfigSource>>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    // Single-port gateway (issue #212): `/__rift/:port/<path>` dispatches to that imposter,
    // so a containerized Rift only needs the one admin port published.
    if let Some(rest) = path.strip_prefix("/__rift/") {
        return handle_gateway(rest, query, req, manager).await;
    }

    // Fast path for common routes
    match (method, path) {
        (&Method::GET, "/") => return system::handle_root(base_url),
        (&Method::GET, "/health") => return system::handle_health(),
        (&Method::GET, "/config") => return system::handle_config(allow_injection),
        (&Method::GET, "/logs") => return system::handle_logs(query),
        (&Method::POST, "/admin/reload") => {
            return system::handle_reload(manager, config_source).await;
        }
        (&Method::GET, "/metrics") => return system::handle_metrics(manager).await,
        _ => {}
    }

    // Imposter collection routes
    if path == "/imposters" {
        return match *method {
            Method::GET => imposters::handle_list(manager, query, base_url).await,
            Method::POST => {
                imposters::handle_create(req, base_url, manager, allow_injection, scripts_dir).await
            }
            Method::PUT => {
                imposters::handle_replace_all(req, base_url, manager, allow_injection, scripts_dir)
                    .await
            }
            Method::DELETE => imposters::handle_delete_all(manager, base_url).await,
            _ => not_found(),
        };
    }

    // Admin flow-state inspection routes: /admin/imposters/:port/flow-state/:flow_id[/:key]
    if let Some(rest) = path.strip_prefix("/admin/imposters/") {
        return route_admin_flow_state(method, rest, req, manager).await;
    }

    // Individual imposter routes
    if let Some(rest) = path.strip_prefix("/imposters/") {
        return route_imposter(
            method,
            rest,
            query,
            req,
            base_url,
            manager,
            allow_injection,
            scripts_dir,
        )
        .await;
    }

    not_found()
}

/// Route `/admin/imposters/:port/flow-state/:flow_id/:key` (issue #190 StateInspection).
async fn route_admin_flow_state(
    method: &Method,
    rest: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [port_str, "flow-state", flow_id, key] => {
            let Ok(port) = port_str.parse::<u16>() else {
                return not_found();
            };
            match *method {
                Method::GET => scenarios::handle_get_flow_state(port, flow_id, key, manager).await,
                Method::PUT => {
                    scenarios::handle_put_flow_state(port, flow_id, key, req, manager).await
                }
                Method::DELETE => {
                    scenarios::handle_delete_flow_state(port, flow_id, key, manager).await
                }
                _ => not_found(),
            }
        }
        _ => not_found(),
    }
}

/// Route imposter-specific requests
#[allow(clippy::too_many_arguments)]
async fn route_imposter(
    method: &Method,
    path: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
) -> Response<Full<Bytes>> {
    // Parse: port/remaining/path
    let segments: Vec<&str> = path.split('/').collect();

    if segments.is_empty() {
        return not_found();
    }

    // Parse port
    let port: u16 = match segments[0].parse() {
        Ok(p) => p,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid port number"),
    };

    // Parse remaining route
    let Some(route) = ImposterRoute::parse(&segments[1..]) else {
        return not_found();
    };

    // Dispatch based on method and route
    match (method, route) {
        // /imposters/:port
        (&Method::GET, ImposterRoute::Root) => {
            imposters::handle_get(port, query, base_url, manager).await
        }
        (&Method::DELETE, ImposterRoute::Root) => {
            imposters::handle_delete(port, base_url, manager).await
        }

        // /imposters/:port/stubs
        (&Method::GET, ImposterRoute::Stubs) => {
            stubs::handle_get_all(port, base_url, manager).await
        }
        (&Method::POST, ImposterRoute::Stubs) => {
            stubs::handle_add(port, req, base_url, manager, allow_injection, scripts_dir).await
        }
        (&Method::PUT, ImposterRoute::Stubs) => {
            stubs::handle_replace_all(port, req, base_url, manager, allow_injection, scripts_dir)
                .await
        }

        // /imposters/:port/stubs/:index
        (&Method::GET, ImposterRoute::StubByIndex(index)) => {
            stubs::handle_get(port, index, base_url, manager).await
        }
        (&Method::PUT, ImposterRoute::StubByIndex(index)) => {
            stubs::handle_replace(
                port,
                index,
                req,
                base_url,
                manager,
                allow_injection,
                scripts_dir,
            )
            .await
        }
        (&Method::DELETE, ImposterRoute::StubByIndex(index)) => {
            stubs::handle_delete(port, index, base_url, manager).await
        }

        // /imposters/:port/stubs/by-id/:id (issue #202)
        (&Method::GET, ImposterRoute::StubById(id)) => {
            stubs::handle_get_by_id(port, &id, manager).await
        }
        (&Method::PUT, ImposterRoute::StubById(id)) => {
            stubs::handle_replace_by_id(
                port,
                &id,
                req,
                base_url,
                manager,
                allow_injection,
                scripts_dir,
            )
            .await
        }
        (&Method::DELETE, ImposterRoute::StubById(id)) => {
            stubs::handle_delete_by_id(port, &id, base_url, manager).await
        }

        // /imposters/:port/savedRequests (alias /requests)
        (&Method::GET, ImposterRoute::SavedRequests) => {
            imposters::handle_get_requests(port, query, manager).await
        }
        (&Method::DELETE, ImposterRoute::SavedRequests) => {
            imposters::handle_clear_requests(port, query, base_url, manager).await
        }

        // /imposters/:port/verify (issue #494)
        (&Method::POST, ImposterRoute::Verify) => {
            imposters::handle_verify(port, req, manager, allow_injection).await
        }

        // /imposters/:port/savedProxyResponses
        (&Method::DELETE, ImposterRoute::SavedProxyResponses) => {
            imposters::handle_clear_proxy_responses(port, base_url, manager).await
        }

        // /imposters/:port/enable, /imposters/:port/disable
        (&Method::POST, ImposterRoute::Enable) => imposters::handle_enable(port, manager).await,
        (&Method::POST, ImposterRoute::Disable) => imposters::handle_disable(port, manager).await,

        // /imposters/:port/scenarios — declarative FSM state inspection/arrangement
        (&Method::GET, ImposterRoute::Scenarios) => {
            scenarios::handle_list_scenarios(port, query, manager).await
        }
        (&Method::PUT, ImposterRoute::ScenarioState(name)) => {
            scenarios::handle_set_scenario_state(port, &name, req, manager).await
        }
        (&Method::POST, ImposterRoute::ScenariosReset) => {
            scenarios::handle_reset_scenarios(port, req, manager).await
        }

        // /imposters/:port/spaces/:flowId — Correlated isolation (issue #223)
        (&Method::POST, ImposterRoute::SpaceStubs(flow_id)) => {
            scenarios::handle_add_space_stub(
                port,
                &flow_id,
                req,
                manager,
                allow_injection,
                scripts_dir,
            )
            .await
        }
        (&Method::GET, ImposterRoute::SpaceStubs(flow_id)) => {
            scenarios::handle_list_space_stubs(port, &flow_id, manager).await
        }
        (&Method::GET, ImposterRoute::Space(flow_id)) => {
            scenarios::handle_get_space(port, &flow_id, manager).await
        }
        (&Method::DELETE, ImposterRoute::Space(flow_id)) => {
            scenarios::handle_teardown_space(port, &flow_id, manager).await
        }

        _ => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_imposter_route_parse() {
        assert!(matches!(
            ImposterRoute::parse(&[]),
            Some(ImposterRoute::Root)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["stubs"]),
            Some(ImposterRoute::Stubs)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["stubs", "0"]),
            Some(ImposterRoute::StubByIndex(0))
        ));
        assert!(matches!(
            ImposterRoute::parse(&["stubs", "5"]),
            Some(ImposterRoute::StubByIndex(5))
        ));
        assert!(matches!(
            ImposterRoute::parse(&["savedRequests"]),
            Some(ImposterRoute::SavedRequests)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["requests"]),
            Some(ImposterRoute::SavedRequests)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["savedProxyResponses"]),
            Some(ImposterRoute::SavedProxyResponses)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["enable"]),
            Some(ImposterRoute::Enable)
        ));
        assert!(matches!(
            ImposterRoute::parse(&["disable"]),
            Some(ImposterRoute::Disable)
        ));

        // Invalid routes
        assert!(ImposterRoute::parse(&["unknown"]).is_none());
        assert!(ImposterRoute::parse(&["stubs", "invalid"]).is_none());
    }
}
