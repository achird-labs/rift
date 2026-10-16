//! The admin listener's route table, exported for embedders (issue #1145).
//!
//! An embedder that fronts this admin API — terminating some routes itself and proxying the rest —
//! needs to know exactly which `(method, path)` pairs the listener dispatches, to check that every
//! one is either handled or forwarded. Without this it has to re-derive the set by reading the
//! router, which is the same "second parser" hazard [`super::authz::classify`] exists to remove.
//!
//! What the tests below and `tests/admin_routes.rs` enforce: every [`ImposterRoute`] variant is
//! listed (a new variant is a compile error in the tests until it is), every authorization action is
//! reached, every entry is dispatched by a live listener, and every *other* method on a listed path
//! is not. A wholly new path outside the per-imposter parser is the one addition they cannot see.

use hyper::Method;

/// Which part of the admin surface a route belongs to.
///
/// Deliberately exhaustive: a new family is a breaking change for an embedder that matches every
/// family, because such an embedder has decided what to do with each — a new one must make it decide
/// again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteFamily {
    /// `/`, `/health`, `/config`, `/logs`, `/metrics`, `/admin/reload`.
    System,
    /// `/imposters` and everything under `/imposters/{port}`.
    Imposters,
    /// `/admin/imposters/{port}/flow-state/…`.
    FlowState,
    /// The server-sent-event streams, dispatched before the router.
    Events,
    /// Served only when the server was built with `with_intercept`; `404` otherwise.
    Intercept,
}

/// One `(method, path)` pair the admin listener dispatches.
///
/// `#[non_exhaustive]` for the same reason as [`super::authz::AuthzTarget`]: upstream produces it and
/// embedders read it, so a field can be added without breaking them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdminRoute {
    /// The one method this entry dispatches. `HEAD` and `OPTIONS` are never listed: every path
    /// answers them, derived from this table — `HEAD` wherever `GET` is listed (issue #1456).
    pub method: Method,
    /// The path, with each parameter in braces under the name [`super::authz::classify`] reports its
    /// value by in `AuthzTarget::params`: `{port}`, `{space}`, `{stubIndex}`, `{stubId}`,
    /// `{scenario}` and `{key}`.
    pub path: &'static str,
    /// Which part of the admin surface it belongs to.
    pub family: RouteFamily,
}

const fn route(method: Method, path: &'static str, family: RouteFamily) -> AdminRoute {
    AdminRoute {
        method,
        path,
        family,
    }
}

use RouteFamily::{Events, FlowState, Imposters, Intercept, System};

/// Every route the admin listener dispatches, grouped by family.
///
/// The single-imposter gateway, `/__rift/{port}/…`, is deliberately absent: it is data-plane
/// traffic to an imposter, not an admin route, which is also why `classify` answers `None` for it.
pub static ADMIN_ROUTES: &[AdminRoute] = &[
    route(Method::GET, "/", System),
    route(Method::GET, "/health", System),
    route(Method::GET, "/config", System),
    route(Method::GET, "/logs", System),
    route(Method::POST, "/admin/reload", System),
    route(Method::GET, "/metrics", System),
    route(Method::GET, "/imposters", Imposters),
    route(Method::POST, "/imposters", Imposters),
    route(Method::PUT, "/imposters", Imposters),
    route(Method::DELETE, "/imposters", Imposters),
    route(Method::GET, "/imposters/{port}", Imposters),
    route(Method::DELETE, "/imposters/{port}", Imposters),
    route(Method::GET, "/imposters/{port}/stubs", Imposters),
    route(Method::POST, "/imposters/{port}/stubs", Imposters),
    route(Method::PUT, "/imposters/{port}/stubs", Imposters),
    route(
        Method::GET,
        "/imposters/{port}/stubs/{stubIndex}",
        Imposters,
    ),
    route(
        Method::PUT,
        "/imposters/{port}/stubs/{stubIndex}",
        Imposters,
    ),
    route(
        Method::DELETE,
        "/imposters/{port}/stubs/{stubIndex}",
        Imposters,
    ),
    route(
        Method::GET,
        "/imposters/{port}/stubs/by-id/{stubId}",
        Imposters,
    ),
    route(
        Method::PUT,
        "/imposters/{port}/stubs/by-id/{stubId}",
        Imposters,
    ),
    route(
        Method::DELETE,
        "/imposters/{port}/stubs/by-id/{stubId}",
        Imposters,
    ),
    route(Method::GET, "/imposters/{port}/savedRequests", Imposters),
    route(Method::DELETE, "/imposters/{port}/savedRequests", Imposters),
    route(Method::GET, "/imposters/{port}/requests", Imposters),
    route(Method::DELETE, "/imposters/{port}/requests", Imposters),
    route(Method::POST, "/imposters/{port}/verify", Imposters),
    route(
        Method::DELETE,
        "/imposters/{port}/savedProxyResponses",
        Imposters,
    ),
    route(Method::POST, "/imposters/{port}/enable", Imposters),
    route(Method::POST, "/imposters/{port}/disable", Imposters),
    route(Method::GET, "/imposters/{port}/scenarios", Imposters),
    route(
        Method::PUT,
        "/imposters/{port}/scenarios/{scenario}/state",
        Imposters,
    ),
    route(Method::POST, "/imposters/{port}/scenarios/reset", Imposters),
    route(Method::GET, "/imposters/{port}/spaces/{space}", Imposters),
    route(
        Method::DELETE,
        "/imposters/{port}/spaces/{space}",
        Imposters,
    ),
    route(
        Method::POST,
        "/imposters/{port}/spaces/{space}/stubs",
        Imposters,
    ),
    route(
        Method::GET,
        "/imposters/{port}/spaces/{space}/stubs",
        Imposters,
    ),
    route(
        Method::DELETE,
        "/admin/imposters/{port}/flow-state/{space}",
        FlowState,
    ),
    route(
        Method::GET,
        "/admin/imposters/{port}/flow-state/{space}/{key}",
        FlowState,
    ),
    route(
        Method::PUT,
        "/admin/imposters/{port}/flow-state/{space}/{key}",
        FlowState,
    ),
    route(
        Method::DELETE,
        "/admin/imposters/{port}/flow-state/{space}/{key}",
        FlowState,
    ),
    route(Method::GET, "/events", Events),
    route(
        Method::GET,
        "/imposters/{port}/savedRequests/stream",
        Events,
    ),
    route(Method::POST, "/intercept", Intercept),
    route(Method::GET, "/intercept", Intercept),
    route(Method::DELETE, "/intercept", Intercept),
    route(Method::POST, "/intercept/rules", Intercept),
    route(Method::PUT, "/intercept/rules", Intercept),
    route(Method::GET, "/intercept/rules", Intercept),
    route(Method::DELETE, "/intercept/rules", Intercept),
    route(Method::GET, "/intercept/ca.pem", Intercept),
    route(Method::GET, "/intercept/truststore.p12", Intercept),
    route(Method::GET, "/intercept/truststore.jks", Intercept),
];

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use super::*;
    use crate::admin_api::authz::classify;
    use crate::admin_api::router::ImposterRoute;
    use crate::extensions::authz::actions;

    /// A concrete path for a template, with the same sample values the integration test uses.
    fn sample(path: &str) -> String {
        path.replace("{port}", "4545")
            .replace("{stubIndex}", "0")
            .replace("{stubId}", "a")
            .replace("{scenario}", "order")
            .replace("{space}", "f1")
            .replace("{key}", "k")
    }

    fn placeholders(path: &str) -> BTreeSet<&str> {
        path.split('/')
            .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
            .collect()
    }

    /// The placeholder names are the names `classify` reports the values under — an embedder reads
    /// the template and then looks the value up by that name.
    #[test]
    fn every_placeholder_is_named_as_classify_reports_it() {
        for entry in ADMIN_ROUTES {
            let target = classify(&entry.method, &sample(entry.path)).expect("classifies");
            let reported: BTreeSet<&str> = target.params.iter().map(|(name, _)| *name).collect();
            assert_eq!(
                placeholders(entry.path),
                reported,
                "{} {}",
                entry.method,
                entry.path
            );
        }
    }

    fn listed(method: &Method, path: &str) -> bool {
        ADMIN_ROUTES
            .iter()
            .any(|r| r.method == *method && r.path == path)
    }

    /// What each `ImposterRoute` variant dispatches — a hand copy of `route_imposter`'s arms.
    ///
    /// No wildcard arm, on purpose: a new variant does not compile until it is given its routes
    /// here, and the assertion below then fails until the table lists them. A new *method* on an
    /// existing variant is caught by `tests/admin_routes.rs`'s unlisted-method sweep instead.
    fn dispatched(route: &ImposterRoute) -> Vec<(Method, &'static str)> {
        match route {
            ImposterRoute::Root => vec![
                (Method::GET, "/imposters/{port}"),
                (Method::DELETE, "/imposters/{port}"),
            ],
            ImposterRoute::Stubs => vec![
                (Method::GET, "/imposters/{port}/stubs"),
                (Method::POST, "/imposters/{port}/stubs"),
                (Method::PUT, "/imposters/{port}/stubs"),
            ],
            ImposterRoute::StubByIndex(_) => vec![
                (Method::GET, "/imposters/{port}/stubs/{stubIndex}"),
                (Method::PUT, "/imposters/{port}/stubs/{stubIndex}"),
                (Method::DELETE, "/imposters/{port}/stubs/{stubIndex}"),
            ],
            ImposterRoute::StubById(_) => vec![
                (Method::GET, "/imposters/{port}/stubs/by-id/{stubId}"),
                (Method::PUT, "/imposters/{port}/stubs/by-id/{stubId}"),
                (Method::DELETE, "/imposters/{port}/stubs/by-id/{stubId}"),
            ],
            // Two spellings, both methods on each.
            ImposterRoute::SavedRequests => vec![
                (Method::GET, "/imposters/{port}/savedRequests"),
                (Method::DELETE, "/imposters/{port}/savedRequests"),
                (Method::GET, "/imposters/{port}/requests"),
                (Method::DELETE, "/imposters/{port}/requests"),
            ],
            ImposterRoute::Verify => vec![(Method::POST, "/imposters/{port}/verify")],
            ImposterRoute::SavedProxyResponses => {
                vec![(Method::DELETE, "/imposters/{port}/savedProxyResponses")]
            }
            ImposterRoute::Enable => vec![(Method::POST, "/imposters/{port}/enable")],
            ImposterRoute::Disable => vec![(Method::POST, "/imposters/{port}/disable")],
            ImposterRoute::Scenarios => vec![(Method::GET, "/imposters/{port}/scenarios")],
            ImposterRoute::ScenarioState(_) => {
                vec![(Method::PUT, "/imposters/{port}/scenarios/{scenario}/state")]
            }
            ImposterRoute::ScenariosReset => {
                vec![(Method::POST, "/imposters/{port}/scenarios/reset")]
            }
            ImposterRoute::Space(_) => vec![
                (Method::GET, "/imposters/{port}/spaces/{space}"),
                (Method::DELETE, "/imposters/{port}/spaces/{space}"),
            ],
            ImposterRoute::SpaceStubs(_) => vec![
                (Method::POST, "/imposters/{port}/spaces/{space}/stubs"),
                (Method::GET, "/imposters/{port}/spaces/{space}/stubs"),
            ],
        }
    }

    #[test]
    fn every_imposter_route_variant_is_in_the_table() {
        let every_variant = [
            ImposterRoute::Root,
            ImposterRoute::Stubs,
            ImposterRoute::StubByIndex(0),
            ImposterRoute::StubById(String::new()),
            ImposterRoute::SavedRequests,
            ImposterRoute::Verify,
            ImposterRoute::SavedProxyResponses,
            ImposterRoute::Enable,
            ImposterRoute::Disable,
            ImposterRoute::Scenarios,
            ImposterRoute::ScenarioState(String::new()),
            ImposterRoute::ScenariosReset,
            ImposterRoute::Space(String::new()),
            ImposterRoute::SpaceStubs(String::new()),
        ];
        for variant in &every_variant {
            for (method, path) in dispatched(variant) {
                assert!(
                    listed(&method, path),
                    "{method} {path} is dispatched but not listed"
                );
            }
        }
    }

    /// The converse: every per-imposter entry names a route the router's own parser accepts.
    /// The stream alias is excluded — the listener dispatches it before the router ever runs.
    #[test]
    fn every_per_imposter_entry_parses_as_an_imposter_route() {
        for entry in ADMIN_ROUTES
            .iter()
            .filter(|r| r.family == Imposters && r.path.starts_with("/imposters/{port}"))
        {
            let concrete = sample(entry.path);
            let rest = concrete
                .strip_prefix("/imposters/4545")
                .expect("sampled port");
            let segments: Vec<&str> = rest.split('/').skip(1).collect();
            assert!(
                ImposterRoute::parse(&segments).is_some(),
                "{} {} does not parse",
                entry.method,
                entry.path
            );
        }
    }

    #[test]
    fn every_entry_is_an_authorizable_admin_route_and_every_action_is_reachable() {
        let mut reached = BTreeSet::new();
        for entry in ADMIN_ROUTES {
            let target = classify(&entry.method, &sample(entry.path))
                .unwrap_or_else(|| panic!("{} {} does not classify", entry.method, entry.path));
            reached.insert(target.action);
        }
        // A list, not a walk: `actions` has no `ALL`, so a tenth action added there would not fail
        // this test. Add it here when it is added there.
        let every_action = BTreeSet::from([
            actions::SYSTEM_READ,
            actions::SYSTEM_WRITE,
            actions::IMPOSTER_READ,
            actions::IMPOSTER_WRITE,
            actions::IMPOSTER_DELETE,
            actions::IMPOSTER_VERIFY,
            actions::EVENTS_READ,
            actions::INTERCEPT_READ,
            actions::INTERCEPT_WRITE,
        ]);
        assert_eq!(reached, every_action);
    }

    /// The drift test (issue #1456): every path in the table resolves, through the router's own
    /// parsers, to exactly the methods listed for it, plus `HEAD` wherever `GET` is listed.
    ///
    /// What it does not catch, and what does: a dispatch arm with no table entry is unreachable,
    /// because dispatch refuses any method `allowed_methods` does not return, so it answers 404
    /// (fails safe) rather than serving an unlisted route. A table entry with no arm fails
    /// `every_table_entry_is_dispatched` in `tests/admin_routes.rs`. A new `ImposterRoute` variant
    /// does not compile until `ImposterRoute::template` names it.
    #[test]
    fn every_table_path_allows_exactly_its_listed_methods() {
        use crate::admin_api::router::allowed_methods;
        for entry in ADMIN_ROUTES {
            let mut listed: Vec<&str> = ADMIN_ROUTES
                .iter()
                .filter(|r| r.path == entry.path)
                .map(|r| r.method.as_str())
                .collect();
            if listed.contains(&"GET") {
                listed.push("HEAD");
            }
            listed.sort_unstable();
            let allowed = allowed_methods(&sample(entry.path))
                .unwrap_or_else(|| panic!("{} does not resolve", entry.path));
            let allowed: Vec<&str> = allowed.iter().map(Method::as_str).collect();
            assert_eq!(allowed, listed, "{}", entry.path);
        }
        assert_eq!(allowed_methods("/nope"), None);
        assert_eq!(allowed_methods("/imposters/4545/nope"), None);
        assert_eq!(allowed_methods("/__rift/4545/orders"), None);
        // One trailing `/` is ignored, as Express ignores it (issue #1481); an empty id is no route.
        assert_eq!(
            allowed_methods("/imposters/"),
            allowed_methods("/imposters")
        );
        assert_eq!(
            allowed_methods("/imposters/4545/stubs/0/"),
            allowed_methods("/imposters/4545/stubs/0")
        );
        assert_eq!(allowed_methods("/imposters//"), None);
        assert_eq!(allowed_methods("/imposters//stubs"), None);
        assert_eq!(allowed_methods("/config//"), None);
    }

    /// `sample(path)` with each literal segment respelled by `spell`; the values are untouched.
    fn respelled(path: &str, spell: fn(&str) -> String) -> String {
        let template: Vec<String> = path
            .split('/')
            .map(|segment| {
                if segment.starts_with('{') {
                    segment.to_string()
                } else {
                    spell(segment)
                }
            })
            .collect();
        sample(&template.join("/"))
    }

    fn alternating(segment: &str) -> String {
        segment
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }

    /// Issue #1484, generated from the table so a new route is covered: a route name in any case
    /// resolves and classifies exactly as its table spelling. `classify` answering `None` skips the
    /// authorizer, so a spelling the router dispatched but `classify` missed would be unauthorized.
    #[test]
    fn a_mixed_case_route_resolves_and_classifies_as_its_table_spelling() {
        use crate::admin_api::router::{admin_path, allowed_methods};
        for entry in ADMIN_ROUTES {
            let plain = sample(entry.path);
            for spell in [str::to_ascii_uppercase, alternating] {
                let mixed = respelled(entry.path, spell);
                for spelling in [mixed.clone(), format!("{mixed}/")] {
                    assert_eq!(admin_path(&spelling), plain, "{spelling}");
                    for method in [entry.method.clone(), Method::OPTIONS] {
                        assert!(classify(&method, &plain).is_some(), "{method} {plain}");
                        assert_eq!(
                            classify(&method, &spelling),
                            classify(&method, &plain),
                            "{method} {spelling}"
                        );
                    }
                    assert_eq!(
                        allowed_methods(&spelling),
                        allowed_methods(&plain),
                        "{spelling}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_route_is_listed_twice() {
        let mut seen = HashSet::new();
        for entry in ADMIN_ROUTES {
            assert!(
                seen.insert((entry.method.clone(), entry.path)),
                "{} {} is listed twice",
                entry.method,
                entry.path
            );
        }
    }

    #[test]
    fn the_gateway_is_not_an_admin_route() {
        assert!(!ADMIN_ROUTES.iter().any(|r| r.path.starts_with("/__rift")));
    }

    /// The families are what an embedder filters on, so each is asserted by its exact membership
    /// count as well as its prefix.
    #[test]
    fn each_family_has_the_routes_it_names() {
        let count = |family| ADMIN_ROUTES.iter().filter(|r| r.family == family).count();
        assert_eq!(count(System), 6);
        assert_eq!(count(Imposters), 30);
        assert_eq!(count(FlowState), 4);
        assert_eq!(count(Events), 2);
        assert_eq!(count(Intercept), 10);
        for entry in ADMIN_ROUTES {
            let prefix_ok = match entry.family {
                Intercept => entry.path.starts_with("/intercept"),
                FlowState => entry.path.starts_with("/admin/imposters/"),
                Imposters => entry.path.starts_with("/imposters"),
                Events => entry.path == "/events" || entry.path.ends_with("/savedRequests/stream"),
                System => {
                    !entry.path.starts_with("/imposters") && !entry.path.starts_with("/intercept")
                }
            };
            assert!(prefix_ok, "{entry:?}");
        }
    }
}
