//! Issue #887: the admin route classifier is reachable from outside the crate.
//!
//! The `AdminAuthorizer` hook (#854) is only usable when upstream parses your routes. An embedder
//! that terminates some admin routes itself and proxies the rest has to build the
//! action/port/space/params tuple to consult its own authorizer — and doing that by hand means a
//! second route parser, which is the divergence `admin_api::authz` exists to prevent.
//!
//! These tests exercise the export the way an embedder would: through the public path only, with
//! no `pub(crate)` reach-in. If `authz` were private again, this file would not compile.

use hyper::Method;
use rift_http_proxy::admin_api::authz::{AuthzTarget, SCOPE_HEADER, classify};
use rift_mock_core::extensions::authz::actions;

#[test]
fn a_parameterised_route_classifies_and_exposes_its_params() {
    let target: AuthzTarget = classify(&Method::PUT, "/imposters/4545/stubs/by-id/abc123")
        .expect("a dispatchable admin route classifies");

    assert_eq!(target.action, actions::IMPOSTER_WRITE);
    assert_eq!(target.port, Some(4545));
    assert!(target.params.contains(&("port", "4545".to_string())));
    assert!(target.params.contains(&("stubId", "abc123".to_string())));
}

#[test]
fn space_routes_expose_the_flow_id_to_an_embedder() {
    let target = classify(&Method::POST, "/imposters/4545/spaces/flow-9/stubs")
        .expect("space route classifies");

    assert_eq!(target.space.as_deref(), Some("flow-9"));
    assert_eq!(target.port, Some(4545));
}

#[test]
fn the_data_plane_is_not_an_authorizable_route() {
    // An embedder's own front must reach the same conclusion upstream does: gateway traffic is
    // app-under-test traffic and is deliberately not admin-gated.
    assert_eq!(classify(&Method::GET, "/__rift/4545/orders"), None);
}

#[test]
fn the_scope_header_name_is_a_constant_rather_than_a_string_to_copy() {
    assert_eq!(SCOPE_HEADER, "x-rift-scope");
}
