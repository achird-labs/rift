use crate::config::{HeaderMatch, HostMatch, Route};
use hyper::Request;
use regex::Regex;

/// Error compiling routing configuration into a [`Router`].
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("Invalid path regex in route '{route}': {source}")]
    InvalidPathRegex {
        route: String,
        #[source]
        source: regex::Error,
    },
}

/// Router matches incoming requests to upstream services
pub struct Router {
    routes: Vec<CompiledRoute>,
}

struct CompiledRoute {
    name: String,
    upstream: String,
    host: Option<CompiledHost>,
    path_prefix: Option<String>,
    path_exact: Option<String>,
    path_regex: Option<Regex>,
    headers: Vec<HeaderMatch>,
}

enum CompiledHost {
    Exact(String),
    Wildcard(String),
}

impl Router {
    /// Create a new router from route configuration
    pub fn new(routes: Vec<Route>) -> Result<Self, RoutingError> {
        let mut compiled = Vec::new();

        for route in routes {
            compiled.push(compile_route(route)?);
        }

        Ok(Router { routes: compiled })
    }

    /// Match a request to an upstream service name
    /// Returns the upstream name if matched, None if no match
    pub fn match_request<B>(&self, req: &Request<B>) -> Option<&str> {
        // First-match-wins algorithm
        for route in &self.routes {
            if matches_route(req, route) {
                return Some(&route.upstream);
            }
        }
        None
    }
}

fn compile_route(route: Route) -> Result<CompiledRoute, RoutingError> {
    let host = route.match_config.host.map(|host_match| match host_match {
        HostMatch::Exact(h) => CompiledHost::Exact(h),
        HostMatch::Wildcard { wildcard } => CompiledHost::Wildcard(wildcard),
    });

    let path_regex = if let Some(pattern) = &route.match_config.path_regex {
        let regex = Regex::new(pattern).map_err(|source| RoutingError::InvalidPathRegex {
            route: route.name.clone(),
            source,
        })?;
        Some(regex)
    } else {
        None
    };

    Ok(CompiledRoute {
        name: route.name,
        upstream: route.upstream,
        host,
        path_prefix: route.match_config.path_prefix,
        path_exact: route.match_config.path_exact,
        path_regex,
        headers: route.match_config.headers,
    })
}

/// Does `host` sit under the wildcard `*.{suffix}`?
///
/// True only for an actual subdomain: there must be at least one non-empty label
/// ending at a literal `.` boundary. The previous `host.ends_with(suffix)` also
/// accepted `example.com` (no label at all) and `evilexample.com` (no boundary) —
/// the latter routing an attacker-chosen host to whatever upstream the route names.
fn is_subdomain_of(host: &str, suffix: &str) -> bool {
    let Some(cut) = host.len().checked_sub(suffix.len()) else {
        return false;
    };
    if !host.is_char_boundary(cut) {
        return false;
    }
    let (label, tail) = host.split_at(cut);
    tail.eq_ignore_ascii_case(suffix) && label.len() > 1 && label.ends_with('.')
}

fn matches_route<B>(req: &Request<B>, route: &CompiledRoute) -> bool {
    // Check host
    if let Some(ref host_match) = route.host {
        let req_host = req
            .uri()
            .host()
            .or_else(|| req.headers().get("host").and_then(|h| h.to_str().ok()));

        // Hostnames are case-insensitive (RFC 4343), as intercept rules already
        // assume (`rift-http-proxy/src/intercept_rules.rs` compares with
        // `eq_ignore_ascii_case`); comparing case-sensitively here gave the same
        // `Host` two different verdicts depending on which matcher saw it.
        let matches = match (req_host, host_match) {
            (Some(req_host), CompiledHost::Exact(pattern)) => {
                req_host.eq_ignore_ascii_case(pattern)
            }
            (Some(req_host), CompiledHost::Wildcard(pattern)) => match pattern.strip_prefix("*.") {
                Some(suffix) => is_subdomain_of(req_host, suffix),
                None => req_host.eq_ignore_ascii_case(pattern),
            },
            _ => false,
        };

        if !matches {
            return false;
        }
    }

    // Check path
    let path = req.uri().path();

    if let Some(ref exact) = route.path_exact
        && path != exact
    {
        return false;
    }

    if let Some(ref prefix) = route.path_prefix
        && !path.starts_with(prefix)
    {
        return false;
    }

    if let Some(ref regex) = route.path_regex
        && !regex.is_match(path)
    {
        return false;
    }

    // Check headers
    for header_match in &route.headers {
        match req.headers().get(&header_match.name) {
            Some(header_val) => {
                if header_val.to_str().ok() != Some(&header_match.value) {
                    return false;
                }
            }
            None => return false,
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteMatch;

    #[test]
    fn test_path_prefix_matching() {
        let routes = vec![Route {
            name: "api".to_string(),
            match_config: RouteMatch {
                path_prefix: Some("/api".to_string()),
                ..Default::default()
            },
            upstream: "api-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://example.com/api/users")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("api-service"));

        let req2 = Request::builder()
            .uri("http://example.com/other")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    #[test]
    fn test_path_exact_matching() {
        let routes = vec![Route {
            name: "exact".to_string(),
            match_config: RouteMatch {
                path_exact: Some("/health".to_string()),
                ..Default::default()
            },
            upstream: "health-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://example.com/health")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("health-service"));

        let req2 = Request::builder()
            .uri("http://example.com/health/check")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    #[test]
    fn test_path_regex_matching() {
        let routes = vec![Route {
            name: "users".to_string(),
            match_config: RouteMatch {
                path_regex: Some(r"^/users/\d+$".to_string()),
                ..Default::default()
            },
            upstream: "user-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://example.com/users/123")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("user-service"));

        let req2 = Request::builder()
            .uri("http://example.com/users/abc")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    #[test]
    fn test_host_exact_matching() {
        let routes = vec![Route {
            name: "api-host".to_string(),
            match_config: RouteMatch {
                host: Some(HostMatch::Exact("api.example.com".to_string())),
                ..Default::default()
            },
            upstream: "api-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://api.example.com/test")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("api-service"));

        let req2 = Request::builder()
            .uri("http://other.example.com/test")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    /// `*.example.com` must match a *subdomain* — not the bare domain, and not a
    /// host that merely ends with the same characters. `ends_with("example.com")`
    /// is true for `evilexample.com`, which routes an attacker-chosen host to
    /// whatever upstream this route names.
    #[test]
    fn wildcard_host_respects_the_label_boundary() {
        let routes = vec![Route {
            name: "subdomain".to_string(),
            match_config: RouteMatch {
                host: Some(HostMatch::Wildcard {
                    wildcard: "*.example.com".to_string(),
                }),
                ..Default::default()
            },
            upstream: "wildcard-service".to_string(),
        }];
        let router = Router::new(routes).unwrap();

        let matches = |host: &str| {
            let req = Request::builder()
                .uri(format!("http://{host}/test"))
                .body(())
                .unwrap();
            router.match_request(&req).is_some()
        };

        assert!(matches("api.example.com"), "a subdomain must match");
        assert!(matches("a.b.example.com"), "a nested subdomain must match");
        assert!(
            !matches("evilexample.com"),
            "a host that merely ends with the suffix must not match: the `.` label \
             boundary is what makes a wildcard a wildcard"
        );
        assert!(
            !matches("example.com"),
            "`*.` requires a label; the bare domain is not a subdomain of itself"
        );
    }

    /// Hostnames are case-insensitive (RFC 4343), and this codebase already knows
    /// that: intercept rules compare with `eq_ignore_ascii_case`
    /// (`rift-http-proxy/src/intercept_rules.rs`). The proxy router comparing
    /// case-sensitively means the same `Host` reaches two different verdicts
    /// depending on which matcher sees it.
    #[test]
    fn host_matching_ignores_case_like_every_other_host_matcher() {
        let routes = vec![
            Route {
                name: "exact".to_string(),
                match_config: RouteMatch {
                    host: Some(HostMatch::Exact("api.example.com".to_string())),
                    ..Default::default()
                },
                upstream: "exact-service".to_string(),
            },
            Route {
                name: "wild".to_string(),
                match_config: RouteMatch {
                    host: Some(HostMatch::Wildcard {
                        wildcard: "*.wild.test".to_string(),
                    }),
                    ..Default::default()
                },
                upstream: "wild-service".to_string(),
            },
        ];
        let router = Router::new(routes).unwrap();

        let upstream_for = |host: &str| {
            let req = Request::builder()
                .uri(format!("http://{host}/test"))
                .body(())
                .unwrap();
            router.match_request(&req).map(str::to_owned)
        };

        assert_eq!(
            upstream_for("API.Example.COM").as_deref(),
            Some("exact-service")
        );
        assert_eq!(upstream_for("A.Wild.TEST").as_deref(), Some("wild-service"));
    }

    #[test]
    fn test_host_wildcard_matching() {
        let routes = vec![Route {
            name: "subdomain".to_string(),
            match_config: RouteMatch {
                host: Some(HostMatch::Wildcard {
                    wildcard: "*.example.com".to_string(),
                }),
                ..Default::default()
            },
            upstream: "wildcard-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://api.example.com/test")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("wildcard-service"));

        let req2 = Request::builder()
            .uri("http://example.org/test")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    #[test]
    fn test_header_matching() {
        let routes = vec![Route {
            name: "versioned".to_string(),
            match_config: RouteMatch {
                headers: vec![HeaderMatch {
                    name: "x-api-version".to_string(),
                    value: "v2".to_string(),
                }],
                ..Default::default()
            },
            upstream: "v2-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://example.com/api")
            .header("x-api-version", "v2")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("v2-service"));

        let req2 = Request::builder()
            .uri("http://example.com/api")
            .header("x-api-version", "v1")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }

    #[test]
    fn test_first_match_wins() {
        let routes = vec![
            Route {
                name: "specific".to_string(),
                match_config: RouteMatch {
                    path_exact: Some("/api/users".to_string()),
                    ..Default::default()
                },
                upstream: "users-service".to_string(),
            },
            Route {
                name: "general".to_string(),
                match_config: RouteMatch {
                    path_prefix: Some("/api".to_string()),
                    ..Default::default()
                },
                upstream: "api-service".to_string(),
            },
        ];

        let router = Router::new(routes).unwrap();

        let req = Request::builder()
            .uri("http://example.com/api/users")
            .body(())
            .unwrap();

        // Should match the first route (specific)
        assert_eq!(router.match_request(&req), Some("users-service"));
    }

    #[test]
    fn test_combined_matching() {
        let routes = vec![Route {
            name: "complex".to_string(),
            match_config: RouteMatch {
                host: Some(HostMatch::Exact("api.example.com".to_string())),
                path_prefix: Some("/v2".to_string()),
                headers: vec![HeaderMatch {
                    name: "authorization".to_string(),
                    value: "Bearer token".to_string(),
                }],
                ..Default::default()
            },
            upstream: "secure-v2-service".to_string(),
        }];

        let router = Router::new(routes).unwrap();

        // All conditions match
        let req = Request::builder()
            .uri("http://api.example.com/v2/users")
            .header("authorization", "Bearer token")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req), Some("secure-v2-service"));

        // Missing header
        let req2 = Request::builder()
            .uri("http://api.example.com/v2/users")
            .body(())
            .unwrap();

        assert_eq!(router.match_request(&req2), None);
    }
}
