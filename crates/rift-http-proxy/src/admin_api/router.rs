//! Route dispatch logic for the Admin API.
//!
//! This module provides routing

use crate::admin_api::handlers::{imposters, intercept, scenarios, stubs, system};
use crate::admin_api::routes::ADMIN_ROUTES;
use crate::admin_api::types::{
    error_response, express_not_found, get_base_url, json_response, not_found,
};
use crate::front_door::FrontDoorRoutes;
use crate::imposter::ImposterManager;
use crate::intercept_control::InterceptControl;
use crate::sources::ReloadSource;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Incoming};
use hyper::header::{ALLOW, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, X_CONTENT_TYPE_OPTIONS};
use hyper::{Method, Request, Response, StatusCode};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

/// Parsed route for imposter-specific endpoints
pub(crate) enum ImposterRoute {
    /// GET/DELETE /imposters/:port
    Root,
    /// POST/PUT/GET /imposters/:port/stubs
    Stubs,
    /// GET/PUT/DELETE /imposters/:port/stubs/:index. The segment is kept as it came: any
    /// non-empty segment names the route, as Express's `:stubIndex` does, and the handler answers
    /// one that is not a current position with Mountebank's `bad data` (issue #1552).
    StubByIndex(String),
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
    pub(crate) fn parse(segments: &[&str]) -> Option<Self> {
        match segments {
            [] => Some(ImposterRoute::Root),
            ["stubs"] => Some(ImposterRoute::Stubs),
            ["stubs", "by-id", id] => Some(ImposterRoute::StubById((*id).to_string())),
            ["stubs", index] if !index.is_empty() => {
                Some(ImposterRoute::StubByIndex((*index).to_string()))
            }
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

    /// The [`ADMIN_ROUTES`] path this route is listed under. No wildcard: a new variant does not
    /// compile until it names its entry, and `allowed_methods` then reads its methods from there.
    fn template(&self) -> &'static str {
        match self {
            ImposterRoute::Root => "/imposters/{port}",
            ImposterRoute::Stubs => "/imposters/{port}/stubs",
            ImposterRoute::StubByIndex(_) => "/imposters/{port}/stubs/{stubIndex}",
            ImposterRoute::StubById(_) => "/imposters/{port}/stubs/by-id/{stubId}",
            // `/requests` is listed with the same methods as this spelling; a routes test pins that.
            ImposterRoute::SavedRequests => "/imposters/{port}/savedRequests",
            ImposterRoute::Verify => "/imposters/{port}/verify",
            ImposterRoute::SavedProxyResponses => "/imposters/{port}/savedProxyResponses",
            ImposterRoute::Enable => "/imposters/{port}/enable",
            ImposterRoute::Disable => "/imposters/{port}/disable",
            ImposterRoute::Scenarios => "/imposters/{port}/scenarios",
            ImposterRoute::ScenarioState(_) => "/imposters/{port}/scenarios/{scenario}/state",
            ImposterRoute::ScenariosReset => "/imposters/{port}/scenarios/reset",
            ImposterRoute::Space(_) => "/imposters/{port}/spaces/{space}",
            ImposterRoute::SpaceStubs(_) => "/imposters/{port}/spaces/{space}/stubs",
        }
    }
}

/// An admin path as the routes see it: the one spelling every consumer matches on.
///
/// This is the shared matcher. The listener rewrites the request with it once
/// ([`normalize_admin_uri`]), before the api-key gate, `authz::classify`, the SSE streams and the
/// router; `classify` and [`template_of`] apply it again for callers that bypass the listener. It is
/// idempotent, and everything downstream of it matches exact-case, so no two consumers can resolve
/// one request to different routes. Mountebank's Express 5 router makes two allowances:
///
/// - **One trailing `/` is dropped**, as Express allows without `strict` (issue #1481). A route
///   matches at most one trailing `/`, so a path ending in `//` names none and is left as it is
///   (except `//`, which Express answers as `/`). Repeated slashes are never collapsed.
/// - **Route names are case-insensitive**, as Express allows without `caseSensitive` (issue
///   #1484): see [`with_route_names_as_listed`]. The values in a route are not.
///
/// The `/__rift/` gateway never comes through here: its path is the stub's own.
pub(crate) fn admin_path(path: &str) -> Cow<'_, str> {
    with_route_names_as_listed(without_trailing_slash(path))
}

fn without_trailing_slash(path: &str) -> &str {
    if path == "//" {
        return "/";
    }
    match path.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() && !stripped.ends_with('/') => stripped,
        _ => path,
    }
}

/// `path` with its route-name segments spelled as [`ADMIN_ROUTES`] spells them (issue #1484).
///
/// A path names a table route when it has the route's segment count and each of the route's
/// literal segments (`imposters`, `stubs`, `savedRequests`, `flow-state`, …) equals the path's
/// segment ignoring ASCII case. A `{param}` segment matches any segment, the empty one included, and
/// is copied through byte for byte: ids, stub ids, space and scenario names and flow-state keys are
/// data, and folding one would name a different resource. A path that names no route is returned
/// unchanged, so it reaches the exact-case parsers as before and is the same unknown route there.
///
/// Unambiguous because no two table paths can name one path: a router test pins that every pair of
/// equally long templates differs in a literal at the same position.
fn with_route_names_as_listed(path: &str) -> Cow<'_, str> {
    let segments: Vec<&str> = path.split('/').collect();
    let Some(template) = ADMIN_ROUTES
        .iter()
        .map(|r| r.path)
        .find(|template| names_template(template, &segments))
    else {
        return Cow::Borrowed(path);
    };
    let respelled: Vec<&str> = template
        .split('/')
        .zip(&segments)
        .map(|(literal, segment)| if is_param(literal) { *segment } else { literal })
        .collect();
    let respelled = respelled.join("/");
    if respelled == path {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(respelled)
    }
}

fn is_param(template_segment: &str) -> bool {
    template_segment.starts_with('{')
}

fn names_template(template: &str, segments: &[&str]) -> bool {
    let literals: Vec<&str> = template.split('/').collect();
    literals.len() == segments.len()
        && literals
            .iter()
            .zip(segments)
            .all(|(literal, segment)| is_param(literal) || literal.eq_ignore_ascii_case(segment))
}

/// Rewrites `req`'s path with [`admin_path`], keeping its query, so every consumer of the path —
/// the api-key gate, `authz::classify`, the SSE streams and the router — sees one spelling.
pub(crate) fn normalize_admin_uri<B>(req: &mut Request<B>) -> Result<(), hyper::http::Error> {
    let path = req.uri().path();
    let normalized = admin_path(path);
    // Compared by content: respelling a route name keeps the length.
    if normalized == path {
        return Ok(());
    }
    let path_and_query = match req.uri().query() {
        Some(query) => format!("{normalized}?{query}"),
        None => normalized.to_string(),
    };
    let mut parts = req.uri().clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse()?);
    *req.uri_mut() = hyper::Uri::from_parts(parts)?;
    Ok(())
}

/// The [`ADMIN_ROUTES`] path a request path names, resolved by the same parsers dispatch uses.
fn template_of(path: &str) -> Option<&'static str> {
    let path = admin_path(path);
    let path = path.as_ref();
    if let Some(entry) = ADMIN_ROUTES
        .iter()
        .find(|r| r.path == path && !r.path.contains('{'))
    {
        return Some(entry.path);
    }
    if let Some(rest) = path.strip_prefix("/admin/imposters/") {
        // Filtered exactly as `route_admin_flow_state` filters.
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        return match segments.as_slice() {
            [port, "flow-state", _] if port.parse::<u16>().is_ok() => {
                Some("/admin/imposters/{port}/flow-state/{space}")
            }
            [port, "flow-state", _, _] if port.parse::<u16>().is_ok() => {
                Some("/admin/imposters/{port}/flow-state/{space}/{key}")
            }
            _ => None,
        };
    }
    let rest = path.strip_prefix("/imposters/")?;
    // Split exactly as `route_imposter` does. The id is not parsed: any id names the route, and
    // one that is not a port names an absent imposter.
    let segments: Vec<&str> = rest.split('/').collect();
    // An empty id (`/imposters//stubs`) names no route: Express's `:id` matches only
    // a non-empty segment, so Mountebank answers the unknown-route 404 on every method.
    if segments[0].is_empty() {
        return None;
    }
    if segments[1..] == ["savedRequests", "stream"] {
        return Some("/imposters/{port}/savedRequests/stream");
    }
    ImposterRoute::parse(&segments[1..]).map(|route| route.template())
}

/// The methods a path answers, sorted, with `HEAD` wherever `GET` is listed: Express's automatic
/// `HEAD`, and the set its automatic `OPTIONS` reports (issue #1456). `None` when no route matches.
///
/// Read from [`ADMIN_ROUTES`], and [`route_request`] dispatches nothing outside it, so the table
/// and what the listener serves cannot drift apart.
pub(crate) fn allowed_methods(path: &str) -> Option<Vec<Method>> {
    let template = template_of(path)?;
    let mut methods: Vec<Method> = ADMIN_ROUTES
        .iter()
        .filter(|r| r.path == template)
        .map(|r| r.method.clone())
        .collect();
    if methods.contains(&Method::GET) {
        methods.push(Method::HEAD);
    }
    methods.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    Some(methods)
}

/// Express 5's automatic `OPTIONS` answer, as Mountebank 2.9.5 sends it.
fn options_response(methods: &[Method]) -> Response<Full<Bytes>> {
    let allow = methods
        .iter()
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    // Method names are header-safe tokens, so this fails only if the table holds a broken one.
    let allow_header = match HeaderValue::from_str(&allow) {
        Ok(value) => value,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("route table method list {allow:?} is not a header value: {e}"),
            );
        }
    };
    let mut response = Response::new(Full::new(Bytes::from(allow)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(ALLOW, allow_header);
    response
}

/// A `GET` answer as `HEAD` sends it: every header, `Content-Length` the body's length, no body.
fn without_body(response: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    let (mut parts, body) = response.into_parts();
    if let Some(length) = body.size_hint().exact()
        && !parts.headers.contains_key(CONTENT_LENGTH)
    {
        parts
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from(length));
    }
    Response::from_parts(parts, Full::new(Bytes::new()))
}

/// The request path as the client sent it, recorded before `normalize_admin_uri` respells the URI,
/// so Express's 404 page can echo it.
#[derive(Clone, Debug)]
pub(crate) struct ReceivedPath(pub(crate) String);

/// Main request router
#[allow(clippy::too_many_arguments)]
pub async fn route_request(
    mut req: Request<Incoming>,
    manager: Arc<ImposterManager>,
    config_source: Option<ReloadSource>,
    front_door_routes: Option<FrontDoorRoutes>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
    config_snapshot: system::ConfigSnapshot,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let mut method = req.method().clone();
    let raw_path = req.uri().path();
    let received_path = req
        .extensions()
        .get::<ReceivedPath>()
        .map_or_else(|| raw_path.to_owned(), |received| received.0.clone());
    let not_found_page = move |method: &Method| {
        let page = express_not_found(method, &received_path);
        if *method == Method::HEAD {
            without_body(page)
        } else {
            page
        }
    };
    // The gateway is matched on the raw path below, before anything is normalized.
    let path = if raw_path.starts_with("/__rift/") {
        raw_path.to_string()
    } else {
        admin_path(raw_path).into_owned()
    };
    let query = req.uri().query().map(|s| s.to_string());
    let base_url = get_base_url(&req);

    debug!("Admin API: {} {}", method, path);

    // Single-port gateway (issue #212): `/__rift/:port/<path>` dispatches to that imposter, so a
    // containerized Rift only needs the one admin port published. It is data-plane traffic, so its
    // `HEAD` and `OPTIONS` belong to the imposter's stubs, not to the automatic answers below.
    if let Some(rest) = path.strip_prefix("/__rift/") {
        return Ok(handle_gateway(rest, query.as_deref(), req, manager).await);
    }

    let is_intercept = path == "/intercept" || path.starts_with("/intercept/");
    if is_intercept && intercept.is_none() {
        return Ok(not_found_page(&method));
    }

    // Express answers `HEAD` and `OPTIONS` on every route by itself (issue #1456), and nothing is
    // dispatched that the route table does not list.
    let Some(allowed) = allowed_methods(&path) else {
        return Ok(not_found_page(&method));
    };
    if method == Method::OPTIONS {
        return Ok(options_response(&allowed));
    }
    if !allowed.contains(&method) {
        return Ok(not_found_page(&method));
    }
    let head = method == Method::HEAD;
    if head {
        method = Method::GET;
        *req.method_mut() = Method::GET;
    }
    let response = dispatch(
        &method,
        &path,
        query.as_deref(),
        req,
        &base_url,
        manager,
        config_source,
        front_door_routes,
        allow_injection,
        intercept,
        scripts_dir,
        config_snapshot,
    )
    .await;
    Ok(if head {
        without_body(response)
    } else {
        response
    })
}

/// Dispatch a request whose method the route table allows on its path.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    method: &Method,
    path: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    base_url: &str,
    manager: Arc<ImposterManager>,
    config_source: Option<ReloadSource>,
    front_door_routes: Option<FrontDoorRoutes>,
    allow_injection: bool,
    intercept: Option<InterceptControl>,
    scripts_dir: Option<Arc<PathBuf>>,
    config_snapshot: system::ConfigSnapshot,
) -> Response<Full<Bytes>> {
    // Intercept routes (epic #394 slice 4 + runtime lifecycle #493): reachable whenever the server
    // was built `with_intercept(...)`. The lifecycle verbs (`POST`/`GET`/`DELETE /intercept`)
    // operate on the shared control slot listener-or-not; the rule/CA sub-routes 404 until a
    // listener is running. Nothing else in `route_by_path` handles `/intercept`, so an unmatched
    // sub-path/method or a control-less server both fall through to the ordinary 404.
    if path == "/intercept" || path.starts_with("/intercept/") {
        return match intercept.as_ref() {
            Some(control) => intercept::route(
                method,
                path,
                query,
                req,
                control,
                allow_injection,
                &manager.body_guard(),
            )
            .await
            .unwrap_or_else(not_found),
            None => not_found(),
        };
    }

    route_by_path(
        method,
        path,
        query,
        req,
        base_url,
        manager,
        config_source,
        front_door_routes,
        intercept.as_ref(),
        allow_injection,
        scripts_dir,
        config_snapshot,
    )
    .await
}

/// Dispatch a `/__rift/:port/<path>` gateway request to the imposter on `:port` (issue #212).
/// Thin wrapper over the library-public `gateway::dispatch_gateway_path` (issue #317), which the
/// front door (issue #19 / U-11) shares for its own fallback addressing so the two listeners
/// cannot drift on what counts as a valid gateway target.
async fn handle_gateway(
    rest: &str,
    query: Option<&str>,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    crate::gateway::dispatch_gateway_path(rest, query, req, &manager).await
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
    config_source: Option<ReloadSource>,
    front_door_routes: Option<FrontDoorRoutes>,
    intercept: Option<&InterceptControl>,
    allow_injection: bool,
    scripts_dir: Option<Arc<PathBuf>>,
    config_snapshot: system::ConfigSnapshot,
) -> Response<Full<Bytes>> {
    // Fast path for common routes
    match (method, path) {
        (&Method::GET, "/") => return system::handle_root(base_url),
        (&Method::GET, "/health") => return system::handle_health(),
        (&Method::GET, "/config") => {
            return system::handle_config(allow_injection, config_snapshot);
        }
        (&Method::GET, "/logs") => {
            return system::handle_logs(query, config_snapshot.log_ring.as_ref());
        }
        (&Method::POST, "/admin/reload") => {
            return system::handle_reload(
                manager,
                config_source,
                front_door_routes.as_ref(),
                intercept,
                allow_injection,
                scripts_dir.as_deref().map(std::path::PathBuf::as_path),
            )
            .await;
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
            Method::DELETE => imposters::handle_delete_all(manager, query, base_url).await,
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

/// Route `/admin/imposters/:port/flow-state/:flow_id[/:key]` — per-key inspection (issue #190
/// StateInspection) and whole-flow invalidation (issue #530).
async fn route_admin_flow_state(
    method: &Method,
    rest: &str,
    req: Request<Incoming>,
    manager: Arc<ImposterManager>,
) -> Response<Full<Bytes>> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        // Whole-flow invalidation (issue #530): DELETE /admin/imposters/:port/flow-state/:flow_id
        [port_str, "flow-state", flow_id] => {
            let Ok(port) = port_str.parse::<u16>() else {
                return not_found();
            };
            match *method {
                Method::DELETE => scenarios::handle_clear_flow_state(port, flow_id, manager).await,
                _ => not_found(),
            }
        }
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

    // An id that is not a port (`abc`, `70000`) cannot name an imposter, so it is answered as one
    // that is absent, as Mountebank answers it (issue #1456) — before any handler or body parse.
    // `route_request` has already resolved the rest of the path to a route that takes `method`.
    let Ok(port) = segments[0].parse::<u16>() else {
        // `allowed_methods` refuses an empty id, but this must not depend on being reached
        // only through it.
        if segments[0].is_empty() {
            return not_found();
        }
        return absent_imposter(method, &segments[1..], segments[0]);
    };

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
            imposters::handle_delete(port, query, base_url, manager).await
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
            stubs::handle_get(port, &index, base_url, manager).await
        }
        (&Method::PUT, ImposterRoute::StubByIndex(index)) => {
            stubs::handle_replace(
                port,
                &index,
                req,
                base_url,
                manager,
                allow_injection,
                scripts_dir,
            )
            .await
        }
        (&Method::DELETE, ImposterRoute::StubByIndex(index)) => {
            stubs::handle_delete(port, &index, base_url, manager).await
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
            imposters::handle_verify(port, req, manager, allow_injection, scripts_dir).await
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

/// What every per-imposter handler answers for a port with no imposter on it: `DELETE
/// /imposters/:id` is `200 {}`, every other route the `404` an absent port gets.
fn absent_imposter(method: &Method, rest: &[&str], id: &str) -> Response<Full<Bytes>> {
    match (method, rest) {
        (&Method::DELETE, []) => json_response(StatusCode::OK, &serde_json::json!({})),
        _ => error_response(
            StatusCode::NOT_FOUND,
            &format!("Imposter not found on port {id}"),
        ),
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
            Some(ImposterRoute::StubByIndex(i)) if i == "0"
        ));
        assert!(matches!(
            ImposterRoute::parse(&["stubs", "5"]),
            Some(ImposterRoute::StubByIndex(i)) if i == "5"
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
        assert!(ImposterRoute::parse(&["stubs", ""]).is_none());
    }

    /// Issue #1552: any non-empty segment names the stub-by-index route, kept verbatim for the
    /// handler to judge, as Express's `:stubIndex` does; the `by-id` route still wins.
    #[test]
    fn any_stub_index_segment_names_the_route() {
        for index in [
            "invalid",
            "abc",
            "01",
            "+1",
            "-1",
            "1.0",
            "18446744073709551616",
        ] {
            assert!(
                matches!(
                    ImposterRoute::parse(&["stubs", index]),
                    Some(ImposterRoute::StubByIndex(ref i)) if i == index
                ),
                "{index}"
            );
            assert_eq!(
                template_of(&format!("/imposters/4545/stubs/{index}")),
                Some("/imposters/{port}/stubs/{stubIndex}"),
                "{index}"
            );
        }
        assert!(matches!(
            ImposterRoute::parse(&["stubs", "by-id", "x"]),
            Some(ImposterRoute::StubById(ref id)) if id == "x"
        ));
        assert_eq!(
            template_of("/imposters/4545/stubs/by-id/x"),
            Some("/imposters/{port}/stubs/by-id/{stubId}")
        );
    }

    #[test]
    fn admin_path_drops_exactly_one_trailing_slash() {
        assert_eq!(admin_path("/"), "/");
        assert_eq!(admin_path("//"), "/");
        assert_eq!(admin_path("/imposters"), "/imposters");
        assert_eq!(admin_path("/imposters/"), "/imposters");
        assert_eq!(admin_path("/imposters//"), "/imposters//");
        assert_eq!(admin_path("///"), "///");
        for path in ["//", "/imposters/", "/imposters//", "/config/", "///"] {
            assert_eq!(admin_path(&admin_path(path)), admin_path(path), "{path}");
        }
        assert_eq!(
            admin_path("/imposters/4545/stubs/0/"),
            "/imposters/4545/stubs/0"
        );
        assert_eq!(admin_path(""), "");
    }

    /// Issue #1484: route names are respelled as the table spells them; values never are.
    #[test]
    fn admin_path_folds_route_names_only() {
        assert_eq!(
            admin_path("/IMPOSTERS/4545/Spaces/Alpha/STUBS/"),
            "/imposters/4545/spaces/Alpha/stubs"
        );
        assert_eq!(
            admin_path("/Imposters/4545/savedrequests/STREAM"),
            "/imposters/4545/savedRequests/stream"
        );
        assert_eq!(
            admin_path("/ADMIN/Imposters/4545/FLOW-STATE/Flow/Key"),
            "/admin/imposters/4545/flow-state/Flow/Key"
        );
        assert_eq!(
            admin_path("/imposters/4545/STUBS/BY-ID/AbC"),
            "/imposters/4545/stubs/by-id/AbC"
        );
        assert_eq!(admin_path("/imposters/AbC/STUBS"), "/imposters/AbC/stubs");
        assert_eq!(admin_path("/INTERCEPT/CA.PEM"), "/intercept/ca.pem");
        // No route: left exactly as sent.
        assert_eq!(admin_path("/IMPOSTERS/4545/NOPE"), "/IMPOSTERS/4545/NOPE");
        assert_eq!(admin_path("/__RIFT/4545/Orders"), "/__RIFT/4545/Orders");
        assert_eq!(admin_path("/IMPOSTERS//"), "/IMPOSTERS//");
        for path in [
            "/IMPOSTERS/",
            "/Imposters/4545/Stubs/0",
            "/imposters/4545/spaces/STUBS",
        ] {
            assert_eq!(admin_path(&admin_path(path)), admin_path(path), "{path}");
        }
    }

    /// What makes [`with_route_names_as_listed`] unambiguous: no path can name two table routes, so
    /// which one it finds first cannot change the spelling. Every pair of equally long templates
    /// must differ, ignoring case, in a literal both have at the same position.
    #[test]
    fn no_path_names_two_table_routes() {
        let mut templates: Vec<&str> = ADMIN_ROUTES.iter().map(|r| r.path).collect();
        templates.sort_unstable();
        templates.dedup();
        for (i, a) in templates.iter().enumerate() {
            for b in &templates[i + 1..] {
                let (a_segments, b_segments): (Vec<&str>, Vec<&str>) =
                    (a.split('/').collect(), b.split('/').collect());
                if a_segments.len() != b_segments.len() {
                    continue;
                }
                let disjoint = a_segments
                    .iter()
                    .zip(&b_segments)
                    .any(|(x, y)| !is_param(x) && !is_param(y) && !x.eq_ignore_ascii_case(y));
                assert!(disjoint, "a path can name both {a} and {b}");
            }
        }
    }

    #[test]
    fn normalize_admin_uri_folds_route_names_and_keeps_the_query() {
        let mut req = Request::get("/IMPOSTERS/4545/SPACES/Alpha?x=Y")
            .body(())
            .expect("request");
        normalize_admin_uri(&mut req).expect("normalizes");
        assert_eq!(req.uri().path(), "/imposters/4545/spaces/Alpha");
        assert_eq!(req.uri().query(), Some("x=Y"));
    }

    #[test]
    fn normalize_admin_uri_keeps_the_query() {
        let mut req = Request::get("/imposters/?replayable=true")
            .body(())
            .expect("request");
        normalize_admin_uri(&mut req).expect("normalizes");
        assert_eq!(req.uri().path(), "/imposters");
        assert_eq!(req.uri().query(), Some("replayable=true"));

        let mut req = Request::get("http://localhost:2525/config/")
            .body(())
            .expect("request");
        normalize_admin_uri(&mut req).expect("normalizes");
        assert_eq!(req.uri().to_string(), "http://localhost:2525/config");
    }
}
