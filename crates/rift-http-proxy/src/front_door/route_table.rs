//! The front door's route table: content-based routing from one listener to many
//! imposters (issue #19 / U-11).
//!
//! # Why this is not `rift_mock_core::extensions::routing`
//!
//! That module routes **reverse-proxy** traffic to a named upstream, and it is
//! the right thing for that job. This one routes **front-door** traffic to an
//! imposter *port*, and three of its rules differ in ways that are not
//! reconcilable by adding a field:
//!
//! - **Order is derived, not authored.** Proxy routes are first-match-wins in
//!   config order. A front-door table is edited over the admin API by several
//!   people and merged from several sources, so "whichever happened to be
//!   written first" is a footgun. Here order is a total function of the routes
//!   themselves — see [`RouteTable::effective_order`].
//! - **Path prefixes are segment-aligned.** `/api/v1` matches `/api/v1/x` but not
//!   `/api/v1x`. The proxy's raw `starts_with` is deliberate there (it pairs with
//!   `path_regex` for precision); here it would silently route a neighbouring
//!   service.
//! - **Targets carry rewrites.** A front-door route can strip its prefix and
//!   override `Host` before the imposter sees the request.
//!
//! What the two *do* share is host matching, and that is imported rather than
//! reimplemented: [`rift_mock_core::extensions::routing::is_subdomain_of`] is the
//! single definition of what `*.example.com` means. Writing a second one is how
//! the label-boundary bug it was extracted from would come back.

use std::cmp::Reverse;

use rift_mock_core::extensions::routing::is_subdomain_of;
use serde::{Deserialize, Serialize};

/// A whole table, as stored and as served over the admin API.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RouteTable {
    #[serde(default)]
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    /// Unique and stable: it is how the admin API addresses one route.
    pub id: String,
    /// Higher wins. Ties fall through to specificity — see
    /// [`RouteTable::effective_order`].
    #[serde(default)]
    pub priority: i32,
    #[serde(default, rename = "match")]
    pub matches: RouteMatch,
    pub target: RouteTarget,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

/// Every clause that is present must match (AND). An empty `RouteMatch` matches
/// everything, which is a legitimate catch-all — and is why ambiguity is
/// rejected at validation rather than resolved silently.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMatch {
    /// Exact (`payments.test`) or one leading wildcard label (`*.payments.test`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Segment-aligned: `/api/v1` matches `/api/v1` and `/api/v1/x`, never
    /// `/api/v1x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Exact values; names compare case-insensitively (HTTP header names are).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeaderMatch {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteTarget {
    pub port: u16,
    /// Strip [`RouteMatch::path_prefix`] before the imposter sees the path.
    /// Defaults to false: predicates and recorded requests see the true path
    /// unless the route asks otherwise.
    #[serde(default)]
    pub strip_prefix: bool,
    /// Rewrite the `Host` the imposter sees. Rare, but recordings show it, so it
    /// is worth being able to control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_host: Option<String>,
}

/// Why a table was refused. Every variant names the offending route(s), because
/// "invalid route table" with a 400 and nothing else is not actionable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteTableError {
    #[error("duplicate route id '{id}'")]
    DuplicateId { id: String },

    #[error(
        "routes '{first}' and '{second}' are both enabled and match exactly the same requests; \
         give one of them a narrower match, or disable one"
    )]
    AmbiguousMatch { first: String, second: String },

    #[error("route '{id}' sets strip_prefix but has no path_prefix to strip")]
    StripWithoutPrefix { id: String },

    #[error(
        "route '{id}' has host '{host}': a wildcard is one leading '*.' label \
         (like '*.payments.test'), and nothing else may contain '*'"
    )]
    MalformedHost { id: String, host: String },

    #[error("route '{id}' has method '{method}', which is not a valid HTTP method")]
    MalformedMethod { id: String, method: String },

    #[error("route '{id}' has path_prefix '{prefix}', which must start with '/'")]
    MalformedPathPrefix { id: String, prefix: String },

    #[error("route id must not be empty")]
    EmptyId,
}

impl RouteTable {
    /// Validate the whole table, rejecting it as a unit.
    ///
    /// Partial acceptance is not offered on purpose: a half-applied routing
    /// table is a topology nobody designed, and the caller cannot tell which
    /// half it got.
    pub fn validate(&self) -> Result<(), RouteTableError> {
        let mut seen_ids = std::collections::HashSet::new();
        for route in &self.routes {
            if route.id.is_empty() {
                return Err(RouteTableError::EmptyId);
            }
            if !seen_ids.insert(route.id.as_str()) {
                return Err(RouteTableError::DuplicateId {
                    id: route.id.clone(),
                });
            }
            route.validate()?;
        }

        // Ambiguity is only a problem between routes that can both win. Two
        // disabled twins, or an enabled route and its disabled spare, are how
        // people stage a change.
        let enabled: Vec<&Route> = self.routes.iter().filter(|r| r.enabled).collect();
        for (i, first) in enabled.iter().enumerate() {
            for second in &enabled[i + 1..] {
                if first.matches == second.matches && first.priority == second.priority {
                    return Err(RouteTableError::AmbiguousMatch {
                        first: first.id.clone(),
                        second: second.id.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The routes in the order they are evaluated: a **total** order, computed
    /// from the routes alone, so the same table always resolves the same way no
    /// matter what order it arrived in.
    ///
    /// `priority` first (the explicit override), then specificity — an exact host
    /// beats a wildcard beats no host; a longer path prefix beats a shorter one;
    /// more header clauses beat fewer — and finally `id`, which is unique, so
    /// there is never a tie left to break arbitrarily.
    pub fn effective_order(&self) -> Vec<&Route> {
        let mut routes: Vec<&Route> = self.routes.iter().filter(|r| r.enabled).collect();
        routes.sort_by(|a, b| {
            Reverse(a.priority)
                .cmp(&Reverse(b.priority))
                .then(a.host_rank().cmp(&b.host_rank()))
                .then(
                    Reverse(a.matches.path_prefix.as_ref().map_or(0, |p| p.len())).cmp(&Reverse(
                        b.matches.path_prefix.as_ref().map_or(0, |p| p.len()),
                    )),
                )
                .then(Reverse(a.matches.headers.len()).cmp(&Reverse(b.matches.headers.len())))
                .then(a.id.cmp(&b.id))
        });
        routes
    }
}

impl Route {
    fn validate(&self) -> Result<(), RouteTableError> {
        if self.target.strip_prefix && self.matches.path_prefix.is_none() {
            return Err(RouteTableError::StripWithoutPrefix {
                id: self.id.clone(),
            });
        }
        if let Some(host) = &self.matches.host {
            let rest = host.strip_prefix("*.").unwrap_or(host);
            if rest.contains('*') || rest.is_empty() {
                return Err(RouteTableError::MalformedHost {
                    id: self.id.clone(),
                    host: host.clone(),
                });
            }
        }
        if let Some(prefix) = &self.matches.path_prefix
            && !prefix.starts_with('/')
        {
            return Err(RouteTableError::MalformedPathPrefix {
                id: self.id.clone(),
                prefix: prefix.clone(),
            });
        }
        if let Some(method) = &self.matches.method
            && method.parse::<hyper::Method>().is_err()
        {
            return Err(RouteTableError::MalformedMethod {
                id: self.id.clone(),
                method: method.clone(),
            });
        }
        Ok(())
    }

    /// Lower is more specific, so it sorts earlier.
    fn host_rank(&self) -> u8 {
        match &self.matches.host {
            Some(h) if h.starts_with("*.") => 1,
            Some(_) => 0,
            None => 2,
        }
    }

    /// Does this route match a request with these properties?
    pub fn matches_request(
        &self,
        host: Option<&str>,
        method: &hyper::Method,
        path: &str,
        headers: &hyper::HeaderMap,
    ) -> bool {
        if let Some(pattern) = &self.matches.host {
            let Some(host) = host else {
                return false;
            };
            let ok = match pattern.strip_prefix("*.") {
                Some(suffix) => is_subdomain_of(host, suffix),
                None => host.eq_ignore_ascii_case(pattern),
            };
            if !ok {
                return false;
            }
        }
        if let Some(expected) = &self.matches.method
            && !method.as_str().eq_ignore_ascii_case(expected)
        {
            return false;
        }
        if let Some(prefix) = &self.matches.path_prefix
            && !path_prefix_matches(path, prefix)
        {
            return false;
        }
        for want in &self.matches.headers {
            match headers.get(&want.name) {
                Some(got) if got.as_bytes() == want.value.as_bytes() => {}
                _ => return false,
            }
        }
        true
    }
}

/// Segment-aligned prefix match: the prefix must end at a path-segment boundary.
///
/// `starts_with` alone would let `/api/v1` claim `/api/v1x`, routing a
/// neighbouring service's traffic to this imposter — the same class of mistake as
/// a host wildcard without a label boundary.
fn path_prefix_matches(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// A table compiled for matching, resolved once per table swap so the per-request
/// path does no sorting and no allocation.
#[derive(Debug, Default)]
pub struct CompiledRoutes {
    ordered: Vec<Route>,
}

impl CompiledRoutes {
    /// Compile a **validated** table. Validation is the caller's job (and the
    /// admission gate's) so that a compile cannot fail at the point where there
    /// is nobody left to report to.
    #[must_use]
    pub fn new(table: &RouteTable) -> Self {
        Self {
            ordered: table.effective_order().into_iter().cloned().collect(),
        }
    }

    /// The first route matching this request in effective order, if any.
    pub fn resolve(
        &self,
        host: Option<&str>,
        method: &hyper::Method,
        path: &str,
        headers: &hyper::HeaderMap,
    ) -> Option<&Route> {
        self.ordered
            .iter()
            .find(|r| r.matches_request(host, method, path, headers))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: &str, port: u16) -> Route {
        Route {
            id: id.to_owned(),
            priority: 0,
            matches: RouteMatch::default(),
            target: RouteTarget {
                port,
                strip_prefix: false,
                set_host: None,
            },
            enabled: true,
        }
    }

    fn with_host(id: &str, port: u16, host: &str) -> Route {
        let mut r = route(id, port);
        r.matches.host = Some(host.to_owned());
        r
    }

    fn with_prefix(id: &str, port: u16, prefix: &str) -> Route {
        let mut r = route(id, port);
        r.matches.path_prefix = Some(prefix.to_owned());
        r
    }

    /// Resolve a `GET` with no headers, returning the winning route's id.
    fn resolve(table: &RouteTable, host: &str, path: &str) -> Option<String> {
        CompiledRoutes::new(table)
            .resolve(
                Some(host),
                &hyper::Method::GET,
                path,
                &hyper::HeaderMap::new(),
            )
            .map(|r| r.id.clone())
    }

    /// A path prefix must end at a segment boundary. Without this, a route for
    /// `/api` swallows `/apiary`, which belongs to somebody else.
    #[test]
    fn path_prefix_is_segment_aligned() {
        assert!(path_prefix_matches("/api/v1", "/api/v1"));
        assert!(path_prefix_matches("/api/v1/users", "/api/v1"));
        assert!(!path_prefix_matches("/api/v1x", "/api/v1"));
        assert!(!path_prefix_matches("/apiary", "/api"));
        // "/" is the catch-all prefix and must still behave.
        assert!(path_prefix_matches("/anything", "/"));
        assert!(path_prefix_matches("/", "/"));
        // A trailing slash in the pattern must not change the meaning.
        assert!(path_prefix_matches("/api/v1/users", "/api/v1/"));
        assert!(!path_prefix_matches("/api/v1x", "/api/v1/"));
    }

    /// The whole point of a derived order: the same routes resolve the same way
    /// regardless of the order they were written or merged in.
    #[test]
    fn effective_order_is_independent_of_input_order() {
        let a = with_host("a-exact", 1, "payments.test");
        let b = with_host("b-wild", 2, "*.payments.test");
        let c = route("c-catchall", 3);

        let forward = RouteTable {
            routes: vec![a.clone(), b.clone(), c.clone()],
        };
        let reverse = RouteTable {
            routes: vec![c, b, a],
        };

        let ids = |t: &RouteTable| {
            t.effective_order()
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&forward), ids(&reverse));
        assert_eq!(ids(&forward), vec!["a-exact", "b-wild", "c-catchall"]);
    }

    /// Specificity beats declaration order: an exact host wins over a wildcard
    /// that also matches, and a longer prefix wins over a shorter one.
    #[test]
    fn more_specific_routes_win() {
        let table = RouteTable {
            routes: vec![
                route("catchall", 1),
                with_host("wild", 2, "*.payments.test"),
                with_host("exact", 3, "api.payments.test"),
            ],
        };
        assert_eq!(
            resolve(&table, "api.payments.test", "/x").as_deref(),
            Some("exact")
        );
        assert_eq!(
            resolve(&table, "other.payments.test", "/x").as_deref(),
            Some("wild")
        );
        assert_eq!(
            resolve(&table, "unrelated.test", "/x").as_deref(),
            Some("catchall")
        );

        let by_prefix = RouteTable {
            routes: vec![
                with_prefix("short", 1, "/api"),
                with_prefix("long", 2, "/api/v1"),
            ],
        };
        assert_eq!(
            resolve(&by_prefix, "h", "/api/v1/users").as_deref(),
            Some("long")
        );
        assert_eq!(
            resolve(&by_prefix, "h", "/api/v2/users").as_deref(),
            Some("short")
        );
    }

    /// `priority` is the explicit override and must beat specificity.
    #[test]
    fn priority_outranks_specificity() {
        let mut catchall = route("catchall", 1);
        catchall.priority = 10;
        let table = RouteTable {
            routes: vec![catchall, with_host("exact", 2, "api.payments.test")],
        };
        assert_eq!(
            resolve(&table, "api.payments.test", "/x").as_deref(),
            Some("catchall")
        );
    }

    /// The host wildcard rule is imported, not reimplemented — this pins that it
    /// is actually the strict one.
    #[test]
    fn wildcard_host_requires_a_real_subdomain() {
        let table = RouteTable {
            routes: vec![with_host("wild", 1, "*.payments.test")],
        };
        assert_eq!(
            resolve(&table, "api.payments.test", "/").as_deref(),
            Some("wild")
        );
        assert_eq!(resolve(&table, "payments.test", "/"), None);
        assert_eq!(resolve(&table, "evilpayments.test", "/"), None);
    }

    #[test]
    fn disabled_routes_never_match() {
        let mut r = with_host("off", 1, "payments.test");
        r.enabled = false;
        let table = RouteTable { routes: vec![r] };
        assert_eq!(resolve(&table, "payments.test", "/"), None);
    }

    #[test]
    fn validation_rejects_duplicate_ids() {
        let table = RouteTable {
            routes: vec![route("same", 1), route("same", 2)],
        };
        assert_eq!(
            table.validate(),
            Err(RouteTableError::DuplicateId {
                id: "same".to_owned()
            })
        );
    }

    /// Two enabled routes that match identically are an authoring mistake with no
    /// right answer, so the table is refused rather than silently resolved by id.
    #[test]
    fn validation_rejects_ambiguous_enabled_routes() {
        let table = RouteTable {
            routes: vec![
                with_host("first", 1, "payments.test"),
                with_host("second", 2, "payments.test"),
            ],
        };
        assert!(matches!(
            table.validate(),
            Err(RouteTableError::AmbiguousMatch { .. })
        ));
    }

    /// ...but staging a replacement by disabling one of them is legitimate, and
    /// differing priority is an explicit tiebreak, so neither is ambiguous.
    #[test]
    fn validation_allows_a_disabled_twin_and_a_priority_tiebreak() {
        let mut spare = with_host("spare", 2, "payments.test");
        spare.enabled = false;
        assert!(
            RouteTable {
                routes: vec![with_host("live", 1, "payments.test"), spare],
            }
            .validate()
            .is_ok()
        );

        let mut higher = with_host("higher", 2, "payments.test");
        higher.priority = 1;
        assert!(
            RouteTable {
                routes: vec![with_host("lower", 1, "payments.test"), higher],
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validation_rejects_strip_without_prefix() {
        let mut r = route("bad", 1);
        r.target.strip_prefix = true;
        assert_eq!(
            RouteTable { routes: vec![r] }.validate(),
            Err(RouteTableError::StripWithoutPrefix {
                id: "bad".to_owned()
            })
        );
    }

    #[test]
    fn validation_rejects_malformed_hosts_and_methods_and_prefixes() {
        let bad_host = |h: &str| {
            RouteTable {
                routes: vec![with_host("r", 1, h)],
            }
            .validate()
        };
        assert!(matches!(
            bad_host("*.*.test"),
            Err(RouteTableError::MalformedHost { .. })
        ));
        assert!(matches!(
            bad_host("pay*.test"),
            Err(RouteTableError::MalformedHost { .. })
        ));
        assert!(matches!(
            bad_host("*."),
            Err(RouteTableError::MalformedHost { .. })
        ));
        assert!(bad_host("*.payments.test").is_ok());
        assert!(bad_host("payments.test").is_ok());

        let mut m = route("r", 1);
        m.matches.method = Some("GHOST METHOD".to_owned());
        assert!(matches!(
            RouteTable { routes: vec![m] }.validate(),
            Err(RouteTableError::MalformedMethod { .. })
        ));

        let mut p = route("r", 1);
        p.matches.path_prefix = Some("api".to_owned());
        assert!(matches!(
            RouteTable { routes: vec![p] }.validate(),
            Err(RouteTableError::MalformedPathPrefix { .. })
        ));
    }

    /// All present clauses AND together.
    #[test]
    fn every_clause_must_match() {
        let mut r = with_host("all", 1, "payments.test");
        r.matches.path_prefix = Some("/api".to_owned());
        r.matches.method = Some("POST".to_owned());
        r.matches.headers = vec![HeaderMatch {
            name: "x-tenant".to_owned(),
            value: "acme".to_owned(),
        }];
        let compiled = CompiledRoutes::new(&RouteTable { routes: vec![r] });

        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-tenant", "acme".parse().unwrap());
        let hit = |host, method: &hyper::Method, path, headers: &hyper::HeaderMap| {
            compiled
                .resolve(Some(host), method, path, headers)
                .is_some()
        };

        assert!(hit(
            "payments.test",
            &hyper::Method::POST,
            "/api/x",
            &headers
        ));
        assert!(!hit("other.test", &hyper::Method::POST, "/api/x", &headers));
        assert!(!hit(
            "payments.test",
            &hyper::Method::GET,
            "/api/x",
            &headers
        ));
        assert!(!hit(
            "payments.test",
            &hyper::Method::POST,
            "/other",
            &headers
        ));
        assert!(!hit(
            "payments.test",
            &hyper::Method::POST,
            "/api/x",
            &hyper::HeaderMap::new()
        ));
    }

    /// Header *names* are case-insensitive per HTTP; values are not.
    #[test]
    fn header_names_are_case_insensitive_values_are_not() {
        let mut r = route("h", 1);
        r.matches.headers = vec![HeaderMatch {
            name: "X-Tenant".to_owned(),
            value: "acme".to_owned(),
        }];
        let compiled = CompiledRoutes::new(&RouteTable { routes: vec![r] });

        let mut lower = hyper::HeaderMap::new();
        lower.insert("x-tenant", "acme".parse().unwrap());
        assert!(
            compiled
                .resolve(None, &hyper::Method::GET, "/", &lower)
                .is_some()
        );

        let mut wrong_value = hyper::HeaderMap::new();
        wrong_value.insert("x-tenant", "ACME".parse().unwrap());
        assert!(
            compiled
                .resolve(None, &hyper::Method::GET, "/", &wrong_value)
                .is_none()
        );
    }

    /// A route with a host clause cannot match a request that has no host at all,
    /// rather than matching vacuously.
    #[test]
    fn a_host_clause_requires_a_host() {
        let compiled = CompiledRoutes::new(&RouteTable {
            routes: vec![with_host("h", 1, "payments.test")],
        });
        assert!(
            compiled
                .resolve(None, &hyper::Method::GET, "/", &hyper::HeaderMap::new())
                .is_none()
        );
    }

    #[test]
    fn empty_table_matches_nothing_and_knows_it() {
        let compiled = CompiledRoutes::new(&RouteTable::default());
        assert!(compiled.is_empty());
        assert!(
            compiled
                .resolve(
                    Some("any.test"),
                    &hyper::Method::GET,
                    "/x",
                    &hyper::HeaderMap::new()
                )
                .is_none()
        );
    }
}
