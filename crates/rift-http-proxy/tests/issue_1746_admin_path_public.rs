//! Issue #1746: an embedder normalises an admin path the way the listener does before it routes, so
//! a front that terminates some routes and forwards the rest matches what the listener would.

use rift_http_proxy::admin_api::{ADMIN_ROUTES, admin_path};

#[test]
fn an_embedder_normalises_admin_paths_as_the_listener_routes_them() {
    assert_eq!(
        admin_path("/imposters/4545/stubs/0/"),
        "/imposters/4545/stubs/0"
    );
    assert_eq!(admin_path("/imposters/"), "/imposters");
    assert_eq!(
        admin_path("/imposters/4545/STUBS/0"),
        "/imposters/4545/stubs/0"
    );
    // A parameter keeps its spelling: a stub id or space name is data.
    assert_eq!(
        admin_path("/IMPOSTERS/4545/Spaces/Alpha/STUBS/"),
        "/imposters/4545/spaces/Alpha/stubs"
    );
    // A path that names no route is left as sent.
    assert_eq!(admin_path("/Not/A/Route/"), "/Not/A/Route");
}

// What the normalised path matches is a route in the exported table, spelled as the table spells it.
#[test]
fn a_normalised_path_matches_its_table_route() {
    let normalised = admin_path("/IMPOSTERS/4545/STUBS/0/");
    let segments: Vec<&str> = normalised.split('/').collect();
    let matched = ADMIN_ROUTES.iter().any(|route| {
        let template: Vec<&str> = route.path.split('/').collect();
        template.len() == segments.len()
            && template
                .iter()
                .zip(&segments)
                .all(|(t, s)| t.starts_with('{') || t == s)
    });
    assert!(matched, "{normalised} names no route in ADMIN_ROUTES");
}
