//! Route classification for the [`AdminAuthorizer`] hook (issue #854).
//!
//! The hook's whole value is that an embedder does not have to re-parse admin routes to decide
//! anything. That means upstream must hand it the route class, the port and the path params it
//! already extracted — which is what [`classify`] produces.
//!
//! Kept as a pure `(method, path) -> Option<AuthzTarget>` function, deliberately separate from
//! dispatch: the mapping from route to permission is the security-relevant part, and it should be
//! testable without standing up a server or a manager.
//!
//! This module is public (issue #887) for the embedder that terminates some admin routes itself
//! and proxies the rest — a shape `ServerBuilder`/`RunningServer` already invite. Such a host gets
//! no classification for free from upstream's `service_fn`, and reimplementing it re-runs an
//! experiment already run here once: a second parser that filtered empty path segments,
//! while the router does not, so `PUT /imposters/:port/scenarios//state` dispatched a mutation
//! that the classifier had never seen. Under `/imposters` that is now unrepresentable — [`classify`]
//! calls the router's own parser — which is precisely the property a hand-written copy loses.
//!
//! [`AdminAuthorizer`]: rift_mock_core::extensions::authz::AdminAuthorizer

use crate::admin_api::router::ImposterRoute;
use hyper::Method;
use rift_mock_core::extensions::authz::actions;

/// Header carrying the embedder-defined scope selector handed to an [`AdminAuthorizer`]
/// (issue #854). Upstream never parses or interprets the value — it exists because an authorizer
/// often cannot derive the target from the port alone (`POST /imposters` has no port yet).
///
/// Public so an embedder's own admin front reads the same header rather than a copied literal
/// (issue #887). It is caller-asserted: any authenticated caller can set it to any value.
///
/// [`AdminAuthorizer`]: rift_mock_core::extensions::authz::AdminAuthorizer
pub const SCOPE_HEADER: &str = "x-rift-scope";

/// A route as the authorizer needs to see it: what is being attempted, and against what.
///
/// Owns its param strings because they are borrowed slices of a path that the caller may not keep
/// alive; [`AuthzRequest`](rift_mock_core::extensions::authz::AuthzRequest) borrows from this.
///
/// `#[non_exhaustive]` for the same reason [`EventContext`] carries it: this is a produced-by-
/// upstream, read-by-the-embedder struct on an opt-in seam, so the next piece of route metadata
/// worth handing an authorizer must not be a breaking change to everyone who destructures one.
/// Reading fields and matching with `..` are unaffected.
///
/// [`EventContext`]: rift_mock_core::imposter::EventContext
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthzTarget {
    /// The action being attempted, e.g. `"imposter.write"` — one of [`actions`].
    ///
    /// [`actions`]: rift_mock_core::extensions::authz::actions
    pub action: &'static str,
    /// The imposter port the route targets. `None` for system routes and for collection routes
    /// that name no port yet (`POST /imposters`).
    pub port: Option<u16>,
    /// The flow/space identifier, for the correlated-isolation routes.
    pub space: Option<String>,
    /// Every path param the router extracted, including `port` and `space` when present, so an
    /// authorizer can key on a route's own vocabulary without re-parsing the path.
    pub params: Vec<(&'static str, String)>,
}

impl AuthzTarget {
    fn new(action: &'static str) -> Self {
        Self {
            action,
            port: None,
            space: None,
            params: Vec::new(),
        }
    }

    fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self.params.push(("port", port.to_string()));
        self
    }

    fn with_space(mut self, space: &str) -> Self {
        self.space = Some(space.to_string());
        self.params.push(("space", space.to_string()));
        self
    }

    fn with_param(mut self, name: &'static str, value: &str) -> Self {
        self.params.push((name, value.to_string()));
        self
    }
}

/// The verb class of a method, for the routes whose permission is purely read-vs-mutate.
///
/// Mechanical rather than hand-assigned per route, so the mapping stays predictable as routes are
/// added and a new route cannot accidentally land on a weaker permission than its siblings.
fn imposter_action(method: &Method) -> &'static str {
    match *method {
        Method::GET | Method::HEAD => actions::IMPOSTER_READ,
        Method::DELETE => actions::IMPOSTER_DELETE,
        _ => actions::IMPOSTER_WRITE,
    }
}

/// Classify an admin request for authorization.
///
/// `None` means "not an authorizable admin route" — the gateway (`/__rift/...`, which is
/// data-plane traffic and deliberately not admin-gated) and paths that match no route at all. An
/// unmatched path falls through to the ordinary 404 without consulting the hook: it reaches no
/// handler and changes nothing, so there is no action to authorize. Note the consequence — an
/// authenticated caller can still distinguish a 404 from a 403, so the hook bounds what a
/// principal can *do*, not what it can learn about which routes exist.
///
/// # Examples
///
/// An embedder fronting the admin API classifies a parameterised route the same way upstream does,
/// and reads the params the handler will act on:
///
/// ```
/// use hyper::Method;
/// use rift_http_proxy::admin_api::authz::classify;
/// use rift_http_proxy::extensions::authz::actions;
///
/// let target = classify(&Method::PUT, "/imposters/4545/scenarios/checkout/state")
///     .expect("a dispatchable admin route classifies");
///
/// assert_eq!(target.action, actions::IMPOSTER_WRITE);
/// assert_eq!(target.port, Some(4545));
/// assert!(target.params.contains(&("scenario", "checkout".to_string())));
///
/// // Data-plane traffic is deliberately not admin-gated.
/// assert_eq!(classify(&Method::GET, "/__rift/4545/orders"), None);
/// ```
#[must_use]
pub fn classify(method: &Method, path: &str) -> Option<AuthzTarget> {
    // Belt-and-braces, and deliberately kept as such: `/__rift/...` matches none of the branches
    // below, so today it would fall through to `None` without this. It is here so that a future
    // catch-all or broadened branch cannot silently start authorizing gateway traffic — that is
    // data-plane imposter traffic, and requiring an admin identity for it would force the app
    // under test to carry the admin credential.
    if path.starts_with("/__rift/") {
        return None;
    }

    match path {
        "/" | "/health" | "/config" | "/logs" | "/metrics" => {
            return Some(AuthzTarget::new(actions::SYSTEM_READ));
        }
        "/admin/reload" => return Some(AuthzTarget::new(actions::SYSTEM_WRITE)),
        _ => {}
    }

    // The SSE streams (issue #461) are dispatched in `service_fn` *before* the router, so they
    // reach no branch below and would otherwise be the one admin surface the hook cannot gate.
    // That is the worst possible gap to leave: `/events` streams recorded method, path, headers
    // and bodies across every imposter, so it is the highest-value read on the whole plane.
    //
    // `/events` gets its own action because it is cross-imposter — there is no port to scope it
    // to, and an authorizer granting `imposter.read` on one port must not thereby leak all of
    // them. The per-imposter alias is an ordinary scoped read.
    if let Some(forced_port) = crate::admin_api::handlers::events::stream_target(path) {
        return Some(match forced_port {
            None => AuthzTarget::new(actions::EVENTS_READ),
            Some(port) => AuthzTarget::new(actions::IMPOSTER_READ).with_port(port),
        });
    }

    if path == "/intercept" || path.starts_with("/intercept/") {
        let action = match *method {
            Method::GET | Method::HEAD => actions::INTERCEPT_READ,
            _ => actions::INTERCEPT_WRITE,
        };
        return Some(AuthzTarget::new(action));
    }

    if path == "/imposters" {
        // `PUT /imposters` reconciles the whole set toward the payload, so `{"imposters":[]}`
        // destroys every imposter. Classifying it by method alone would call that `imposter.write`
        // and hand a "may write, may not delete" principal a trivial way round the restriction.
        let action = if method == Method::PUT {
            actions::IMPOSTER_DELETE
        } else {
            // No port yet on a create — this is exactly the case the `scope` field exists for.
            imposter_action(method)
        };
        return Some(AuthzTarget::new(action));
    }

    if let Some(rest) = path.strip_prefix("/admin/imposters/") {
        return classify_admin_flow_state(method, rest);
    }

    if let Some(rest) = path.strip_prefix("/imposters/") {
        return classify_imposter(method, rest);
    }

    None
}

/// `/admin/imposters/:port/flow-state/:flow_id[/:key]`
fn classify_admin_flow_state(method: &Method, rest: &str) -> Option<AuthzTarget> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let (port_str, flow_id, key) = match segments.as_slice() {
        [port, "flow-state", flow_id] => (port, *flow_id, None),
        [port, "flow-state", flow_id, key] => (port, *flow_id, Some(*key)),
        _ => return None,
    };
    let port: u16 = port_str.parse().ok()?;
    let mut target = AuthzTarget::new(imposter_action(method))
        .with_port(port)
        .with_space(flow_id);
    if let Some(key) = key {
        target = target.with_param("key", key);
    }
    Some(target)
}

/// `/imposters/:port/...`
///
/// Delegates to the router's own [`ImposterRoute::parse`] rather than re-deriving the route.
/// A second parser is a security bug waiting to happen: this one used to filter empty segments
/// while the router does not, so `PUT /imposters/:port/scenarios//state` dispatched into a real
/// mutation that the classifier had never seen, and `POST /imposters/:port/spaces//stubs` was
/// authorized against the space `"stubs"` while writing into `""`. Sharing the parser makes that
/// class of divergence impossible, and the exhaustive match below turns a new route variant into
/// a compile error instead of a silently unauthorized handler.
fn classify_imposter(method: &Method, rest: &str) -> Option<AuthzTarget> {
    // Split exactly as `route_imposter` does — no `filter`. Hyper does not normalise `//` or a
    // trailing `/`, so an empty segment reaches here and must reach the router's parser too.
    let segments: Vec<&str> = rest.split('/').collect();
    let port: u16 = segments.first()?.parse().ok()?;
    let route = ImposterRoute::parse(&segments[1..])?;

    let base = |action: &'static str| AuthzTarget::new(action).with_port(port);

    let target = match route {
        ImposterRoute::Root
        | ImposterRoute::Stubs
        | ImposterRoute::SavedRequests
        | ImposterRoute::SavedProxyResponses
        | ImposterRoute::Scenarios => base(imposter_action(method)),

        // A POST that mutates nothing: it asserts against already-recorded requests. Mapping it
        // to `imposter.write` on method alone would stop a read-only principal verifying.
        ImposterRoute::Verify => base(actions::IMPOSTER_VERIFY),

        ImposterRoute::Enable | ImposterRoute::Disable | ImposterRoute::ScenariosReset => {
            base(actions::IMPOSTER_WRITE)
        }

        ImposterRoute::StubByIndex(index) => {
            base(imposter_action(method)).with_param("stubIndex", &index.to_string())
        }
        ImposterRoute::StubById(id) => base(imposter_action(method)).with_param("stubId", &id),

        ImposterRoute::ScenarioState(name) => {
            base(actions::IMPOSTER_WRITE).with_param("scenario", &name)
        }

        ImposterRoute::Space(flow_id) | ImposterRoute::SpaceStubs(flow_id) => {
            base(imposter_action(method)).with_space(&flow_id)
        }
    };
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_of(method: Method, path: &str) -> Option<&'static str> {
        classify(&method, path).map(|t| t.action)
    }

    #[test]
    fn system_routes_are_reads_and_reload_is_a_write() {
        for p in ["/", "/health", "/config", "/logs", "/metrics"] {
            assert_eq!(action_of(Method::GET, p), Some(actions::SYSTEM_READ), "{p}");
        }
        assert_eq!(
            action_of(Method::POST, "/admin/reload"),
            Some(actions::SYSTEM_WRITE)
        );
    }

    #[test]
    fn the_gateway_is_not_an_admin_route() {
        // `/__rift/...` is data-plane imposter traffic and is deliberately not admin-gated;
        // authorizing it would force app-under-test traffic to carry an admin identity.
        //
        // Note this test does not currently distinguish the explicit guard in `classify` from the
        // fall-through — deleting the guard keeps it green, because no other branch matches
        // `/__rift/` either. It pins the *contract*, and it starts biting the moment anyone adds a
        // catch-all that would otherwise swallow gateway paths.
        assert_eq!(classify(&Method::GET, "/__rift/4545/orders"), None);
        assert_eq!(classify(&Method::POST, "/__rift/4545/orders"), None);
    }

    #[test]
    fn unknown_paths_are_not_authorizable() {
        assert_eq!(classify(&Method::GET, "/nope"), None);
        assert_eq!(classify(&Method::GET, "/imposters/abc"), None);
        assert_eq!(classify(&Method::GET, "/imposters/4545/nope"), None);
        // A non-numeric stub index is not a route.
        assert_eq!(classify(&Method::GET, "/imposters/4545/stubs/xyz"), None);
    }

    #[test]
    fn method_decides_read_write_delete_on_imposter_routes() {
        assert_eq!(
            action_of(Method::GET, "/imposters/4545"),
            Some(actions::IMPOSTER_READ)
        );
        assert_eq!(
            action_of(Method::DELETE, "/imposters/4545"),
            Some(actions::IMPOSTER_DELETE)
        );
        assert_eq!(
            action_of(Method::PUT, "/imposters/4545/stubs"),
            Some(actions::IMPOSTER_WRITE)
        );
        assert_eq!(
            action_of(Method::POST, "/imposters"),
            Some(actions::IMPOSTER_WRITE)
        );
        assert_eq!(
            action_of(Method::DELETE, "/imposters"),
            Some(actions::IMPOSTER_DELETE)
        );
    }

    #[test]
    fn verify_is_not_a_write_despite_being_a_post() {
        assert_eq!(
            action_of(Method::POST, "/imposters/4545/verify"),
            Some(actions::IMPOSTER_VERIFY)
        );
    }

    #[test]
    fn enable_and_disable_are_writes_despite_carrying_no_body() {
        assert_eq!(
            action_of(Method::POST, "/imposters/4545/enable"),
            Some(actions::IMPOSTER_WRITE)
        );
        assert_eq!(
            action_of(Method::POST, "/imposters/4545/disable"),
            Some(actions::IMPOSTER_WRITE)
        );
    }

    #[test]
    fn create_has_no_port_so_scope_is_the_only_target_selector() {
        // The issue's stated reason for `scope` existing: POST /imposters names no port.
        let t = classify(&Method::POST, "/imposters").unwrap();
        assert_eq!(t.port, None);
        assert!(t.params.is_empty());
    }

    #[test]
    fn port_is_extracted_and_also_exposed_as_a_param() {
        let t = classify(&Method::GET, "/imposters/4545/stubs").unwrap();
        assert_eq!(t.port, Some(4545));
        assert!(t.params.contains(&("port", "4545".to_string())));
    }

    #[test]
    fn path_params_reach_the_authorizer() {
        let by_id = classify(&Method::PUT, "/imposters/4545/stubs/by-id/abc123").unwrap();
        assert!(by_id.params.contains(&("stubId", "abc123".to_string())));

        let by_index = classify(&Method::DELETE, "/imposters/4545/stubs/7").unwrap();
        assert!(by_index.params.contains(&("stubIndex", "7".to_string())));

        let scenario = classify(&Method::PUT, "/imposters/4545/scenarios/checkout/state").unwrap();
        assert!(
            scenario
                .params
                .contains(&("scenario", "checkout".to_string()))
        );
    }

    #[test]
    fn empty_segments_are_classified_rather_than_filtered_away() {
        // The regression that makes a single parser non-negotiable, pinned. Hyper does not
        // normalise `//`, so a classifier that filtered empty segments saw a different route from
        // the one the router dispatched: the mutation ran, authorized as something else or never
        // classified at all. Reintroducing the filter makes the first `expect` panic (no route
        // matches `["scenarios", "state"]`) and the second assert read `Some("stubs")`.
        let scenario = classify(&Method::PUT, "/imposters/4545/scenarios//state")
            .expect("the router dispatches this, so it must classify");
        assert_eq!(scenario.action, actions::IMPOSTER_WRITE);
        assert!(scenario.params.contains(&("scenario", String::new())));

        let space = classify(&Method::POST, "/imposters/4545/spaces//stubs")
            .expect("the router dispatches this, so it must classify");
        assert_eq!(space.space.as_deref(), Some(""));
    }

    #[test]
    fn the_flow_state_branch_filters_empty_segments_exactly_as_its_router_twin_does() {
        // `classify_admin_flow_state` is the one branch that does NOT delegate to the router — it
        // hand-copies `route_admin_flow_state`, filter included. They agree today; this pins the
        // agreement, because that is the branch where the divergence above could come back.
        let t = classify(&Method::DELETE, "/admin/imposters/4545/flow-state//cart")
            .expect("the filter collapses the empty segment, so this is the 3-segment route");
        assert_eq!(t.space.as_deref(), Some("cart"));
        assert!(!t.params.iter().any(|(name, _)| *name == "key"));
    }

    #[test]
    fn space_routes_expose_the_flow_id() {
        let t = classify(&Method::POST, "/imposters/4545/spaces/flow-9/stubs").unwrap();
        assert_eq!(t.space.as_deref(), Some("flow-9"));
        assert_eq!(t.port, Some(4545));

        let s = classify(&Method::DELETE, "/imposters/4545/spaces/flow-9").unwrap();
        assert_eq!(s.space.as_deref(), Some("flow-9"));
        assert_eq!(s.action, actions::IMPOSTER_DELETE);
    }

    #[test]
    fn flow_state_inspection_carries_port_space_and_key() {
        let t = classify(&Method::GET, "/admin/imposters/4545/flow-state/flow-9/cart").unwrap();
        assert_eq!(t.port, Some(4545));
        assert_eq!(t.space.as_deref(), Some("flow-9"));
        assert!(t.params.contains(&("key", "cart".to_string())));
        assert_eq!(t.action, actions::IMPOSTER_READ);

        let whole = classify(&Method::DELETE, "/admin/imposters/4545/flow-state/flow-9").unwrap();
        assert_eq!(whole.action, actions::IMPOSTER_DELETE);
        assert!(!whole.params.iter().any(|(n, _)| *n == "key"));
    }

    #[test]
    fn intercept_routes_split_read_from_write() {
        assert_eq!(
            action_of(Method::GET, "/intercept"),
            Some(actions::INTERCEPT_READ)
        );
        assert_eq!(
            action_of(Method::POST, "/intercept"),
            Some(actions::INTERCEPT_WRITE)
        );
        assert_eq!(
            action_of(Method::DELETE, "/intercept/rules"),
            Some(actions::INTERCEPT_WRITE)
        );
    }

    #[test]
    fn the_requests_alias_classifies_like_saved_requests() {
        // `/requests` is an alias for `/savedRequests`; a permission difference between them
        // would be a bypass of whichever is stricter.
        assert_eq!(
            action_of(Method::DELETE, "/imposters/4545/requests"),
            action_of(Method::DELETE, "/imposters/4545/savedRequests")
        );
        assert_eq!(
            action_of(Method::GET, "/imposters/4545/requests"),
            action_of(Method::GET, "/imposters/4545/savedRequests")
        );
    }

    #[test]
    fn every_dispatchable_imposter_route_classifies() {
        // A route the router dispatches but `classify` returns None for would reach its handler
        // without ever consulting the authorizer — a silent hole. Mirrors ImposterRoute::parse.
        let routes = [
            (Method::GET, ""),
            (Method::DELETE, ""),
            (Method::GET, "/stubs"),
            (Method::POST, "/stubs"),
            (Method::PUT, "/stubs"),
            (Method::GET, "/stubs/0"),
            (Method::PUT, "/stubs/0"),
            (Method::DELETE, "/stubs/0"),
            (Method::GET, "/stubs/by-id/x"),
            (Method::PUT, "/stubs/by-id/x"),
            (Method::DELETE, "/stubs/by-id/x"),
            (Method::GET, "/savedRequests"),
            (Method::DELETE, "/savedRequests"),
            (Method::GET, "/requests"),
            (Method::POST, "/verify"),
            (Method::DELETE, "/savedProxyResponses"),
            (Method::POST, "/enable"),
            (Method::POST, "/disable"),
            (Method::GET, "/scenarios"),
            (Method::PUT, "/scenarios/s/state"),
            (Method::POST, "/scenarios/reset"),
            (Method::GET, "/spaces/f"),
            (Method::DELETE, "/spaces/f"),
            (Method::POST, "/spaces/f/stubs"),
            (Method::GET, "/spaces/f/stubs"),
        ];
        for (method, tail) in routes {
            let path = format!("/imposters/4545{tail}");
            assert!(
                classify(&method, &path).is_some(),
                "{method} {path} dispatches but does not classify"
            );
        }
    }
}
