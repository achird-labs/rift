//! Tests for the proxy module.
//!
//! This module contains integration and unit tests for the proxy server components.

#[cfg(test)]
mod script_pool_tests {
    use crate::scripting::ScriptPoolConfig;

    #[test]
    fn test_script_pool_config_creation() {
        let config = ScriptPoolConfig::default();
        assert!(config.workers >= 2);
        assert!(config.workers <= 16);
    }
}

#[cfg(test)]
mod decision_cache_tests {
    use crate::scripting::DecisionCacheConfig;

    #[test]
    fn test_decision_cache_config_creation() {
        let config = DecisionCacheConfig::default();
        assert!(config.enabled);
        assert!(config.max_size > 0);
        assert!(config.ttl_seconds > 0);
    }
}

#[cfg(test)]
mod recording_tests {
    use crate::recording::ProxyMode;

    #[test]
    fn test_proxy_mode_default() {
        let mode = ProxyMode::default();
        assert_eq!(mode, ProxyMode::ProxyTransparent);
    }

    #[test]
    fn test_proxy_mode_variants() {
        assert_ne!(ProxyMode::ProxyOnce, ProxyMode::ProxyAlways);
        assert_ne!(ProxyMode::ProxyOnce, ProxyMode::ProxyTransparent);
        assert_ne!(ProxyMode::ProxyAlways, ProxyMode::ProxyTransparent);
    }
}

#[cfg(test)]
mod router_tests {
    use crate::config::{Route, RouteMatch};
    use crate::extensions::routing::Router;

    #[test]
    fn test_router_creation_empty() {
        let router = Router::new(vec![]);
        assert!(router.is_ok());
    }

    #[test]
    fn test_router_creation_with_rules() {
        let routes = vec![Route {
            name: "api-route".to_string(),
            upstream: "backend-a".to_string(),
            match_config: RouteMatch {
                path_prefix: Some("/api".to_string()),
                ..Default::default()
            },
        }];
        let router = Router::new(routes);
        assert!(router.is_ok());
    }

    #[test]
    fn test_router_path_prefix_matching() {
        let routes = vec![
            Route {
                name: "v1-route".to_string(),
                upstream: "backend-a".to_string(),
                match_config: RouteMatch {
                    path_prefix: Some("/api/v1".to_string()),
                    ..Default::default()
                },
            },
            Route {
                name: "v2-route".to_string(),
                upstream: "backend-b".to_string(),
                match_config: RouteMatch {
                    path_prefix: Some("/api/v2".to_string()),
                    ..Default::default()
                },
            },
        ];
        let router = Router::new(routes).unwrap();

        // Create test request
        let req = hyper::Request::builder()
            .uri("http://localhost/api/v1/users")
            .body(())
            .unwrap();
        let matched = router.match_request(&req);
        assert_eq!(matched, Some("backend-a"));

        let req2 = hyper::Request::builder()
            .uri("http://localhost/api/v2/items")
            .body(())
            .unwrap();
        let matched2 = router.match_request(&req2);
        assert_eq!(matched2, Some("backend-b"));
    }

    #[test]
    fn test_router_no_match() {
        let routes = vec![Route {
            name: "api-route".to_string(),
            upstream: "backend-a".to_string(),
            match_config: RouteMatch {
                path_prefix: Some("/api".to_string()),
                ..Default::default()
            },
        }];
        let router = Router::new(routes).unwrap();

        let req = hyper::Request::builder()
            .uri("http://localhost/other/path")
            .body(())
            .unwrap();
        let matched = router.match_request(&req);
        assert_eq!(matched, None);
    }

    #[test]
    fn test_router_exact_path_matching() {
        let routes = vec![Route {
            name: "exact-route".to_string(),
            upstream: "backend-exact".to_string(),
            match_config: RouteMatch {
                path_exact: Some("/exact/path".to_string()),
                ..Default::default()
            },
        }];
        let router = Router::new(routes).unwrap();

        let req = hyper::Request::builder()
            .uri("http://localhost/exact/path")
            .body(())
            .unwrap();
        assert_eq!(router.match_request(&req), Some("backend-exact"));

        let req2 = hyper::Request::builder()
            .uri("http://localhost/exact/path/extra")
            .body(())
            .unwrap();
        assert_eq!(router.match_request(&req2), None);
    }
}

#[cfg(test)]
mod compiled_rule_tests {
    use crate::config::{FaultConfig, MatchConfig, PathMatch, Rule};
    use crate::extensions::matcher::CompiledRule;
    use hyper::{HeaderMap, Method, Uri};

    fn create_rule(path: PathMatch, methods: Vec<&str>) -> Rule {
        Rule {
            id: "test-rule".to_string(),
            match_config: MatchConfig {
                methods: methods.iter().map(|m| m.to_string()).collect(),
                path,
                headers: vec![],
                header_predicates: vec![],
                query: vec![],
                body: None,
                case_sensitive: true,
            },
            fault: FaultConfig::default(),
            upstream: None,
        }
    }

    #[test]
    fn test_compiled_rule_any_path() {
        let rule = create_rule(PathMatch::Any, vec!["GET"]);
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri: Uri = "http://localhost/any/path/here".parse().unwrap();
        let headers = HeaderMap::new();
        assert!(compiled.matches(&Method::GET, &uri, &headers));
    }

    #[test]
    fn test_compiled_rule_exact_path() {
        let rule = create_rule(
            PathMatch::Exact {
                exact: "/exact/path".to_string(),
            },
            vec!["POST"],
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri: Uri = "http://localhost/exact/path".parse().unwrap();
        let headers = HeaderMap::new();
        assert!(compiled.matches(&Method::POST, &uri, &headers));

        let uri2: Uri = "http://localhost/exact/path/extra".parse().unwrap();
        assert!(!compiled.matches(&Method::POST, &uri2, &headers));
    }

    #[test]
    fn test_compiled_rule_prefix_path() {
        let rule = create_rule(
            PathMatch::Prefix {
                prefix: "/api/".to_string(),
            },
            vec![],
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri1: Uri = "http://localhost/api/users".parse().unwrap();
        let uri2: Uri = "http://localhost/api/items/123".parse().unwrap();
        let uri3: Uri = "http://localhost/other".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_compiled_rule_regex_path() {
        let rule = create_rule(
            PathMatch::Regex {
                regex: r"^/api/v\d+/.*".to_string(),
            },
            vec![],
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri1: Uri = "http://localhost/api/v1/users".parse().unwrap();
        let uri2: Uri = "http://localhost/api/v2/items".parse().unwrap();
        let uri3: Uri = "http://localhost/api/users".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_compiled_rule_contains_path() {
        let rule = create_rule(
            PathMatch::Contains {
                contains: "admin".to_string(),
            },
            vec![],
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri1: Uri = "http://localhost/api/admin/users".parse().unwrap();
        let uri2: Uri = "http://localhost/admin".parse().unwrap();
        let uri3: Uri = "http://localhost/api/users".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_compiled_rule_ends_with_path() {
        let rule = create_rule(
            PathMatch::EndsWith {
                ends_with: ".json".to_string(),
            },
            vec![],
        );
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri1: Uri = "http://localhost/api/data.json".parse().unwrap();
        let uri2: Uri = "http://localhost/config.json".parse().unwrap();
        let uri3: Uri = "http://localhost/api/data.xml".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::GET, &uri1, &headers));
        assert!(compiled.matches(&Method::GET, &uri2, &headers));
        assert!(!compiled.matches(&Method::GET, &uri3, &headers));
    }

    #[test]
    fn test_compiled_rule_multiple_methods() {
        let rule = create_rule(PathMatch::Any, vec!["GET", "POST", "PUT"]);
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri: Uri = "http://localhost/test".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(compiled.matches(&Method::GET, &uri, &headers));
        assert!(compiled.matches(&Method::POST, &uri, &headers));
        assert!(compiled.matches(&Method::PUT, &uri, &headers));
        assert!(!compiled.matches(&Method::DELETE, &uri, &headers));
    }

    #[test]
    fn test_compiled_rule_empty_methods_matches_all() {
        let rule = create_rule(PathMatch::Any, vec![]);
        let compiled = CompiledRule::compile(rule).unwrap();

        let uri: Uri = "http://localhost/test".parse().unwrap();
        let headers = HeaderMap::new();

        // Empty methods list should match all methods
        assert!(compiled.matches(&Method::GET, &uri, &headers));
        assert!(compiled.matches(&Method::POST, &uri, &headers));
        assert!(compiled.matches(&Method::DELETE, &uri, &headers));
        assert!(compiled.matches(&Method::PATCH, &uri, &headers));
    }
}

#[cfg(test)]
mod flow_store_tests {
    use crate::extensions::flow_state::{FlowStore, NoOpFlowStore};
    use serde_json::json;

    #[test]
    fn test_noop_flow_store_get() {
        let store = NoOpFlowStore;
        let result = store.get("flow-1", "key").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_noop_flow_store_set() {
        let store = NoOpFlowStore;
        let result = store.set("flow-1", "key", json!({"value": 42}));
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_flow_store_exists() {
        let store = NoOpFlowStore;
        let result = store.exists("flow-1", "key").unwrap();
        assert!(!result);
    }

    #[test]
    fn test_noop_flow_store_delete() {
        let store = NoOpFlowStore;
        let result = store.delete("flow-1", "key");
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_flow_store_increment() {
        let store = NoOpFlowStore;
        let result = store.increment("flow-1", "counter").unwrap();
        // NoOpFlowStore always returns 1 for increment
        assert_eq!(result, 1);
    }

    #[test]
    fn test_noop_flow_store_set_ttl() {
        let store = NoOpFlowStore;
        let result = store.set_ttl("flow-1", 3600);
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod behavior_state_tests {
    use crate::behaviors::{CsvCache, ResponseCycler};

    #[test]
    fn test_response_cycler_creation() {
        let cycler = ResponseCycler::new();
        // Just verify it can be created
        assert!(std::mem::size_of_val(&cycler) > 0);
    }

    #[test]
    fn test_csv_cache_creation() {
        let cache = CsvCache::new();
        // Just verify it can be created
        assert!(std::mem::size_of_val(&cache) > 0);
    }
}

#[cfg(test)]
mod recording_store_tests {
    use crate::recording::{ProxyMode, RecordedResponse, RecordingStore, RequestSignature};

    #[test]
    fn test_recording_store_transparent_mode() {
        let store = RecordingStore::new(ProxyMode::ProxyTransparent);
        assert_eq!(store.mode(), ProxyMode::ProxyTransparent);
    }

    #[test]
    fn test_recording_store_proxy_once_mode() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        assert_eq!(store.mode(), ProxyMode::ProxyOnce);
    }

    #[test]
    fn test_recording_store_proxy_always_mode() {
        let store = RecordingStore::new(ProxyMode::ProxyAlways);
        assert_eq!(store.mode(), ProxyMode::ProxyAlways);
    }

    #[test]
    fn test_request_signature_creation() {
        let sig = RequestSignature::new("GET", "/api/users", Some("page=1"), &[]);
        assert!(std::mem::size_of_val(&sig) > 0);
    }

    #[test]
    fn test_request_signature_with_headers() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer token".to_string()),
            ("X-Custom".to_string(), "value".to_string()),
        ];
        let sig = RequestSignature::new("POST", "/api/data", None, &headers);
        assert!(std::mem::size_of_val(&sig) > 0);
    }

    #[test]
    fn test_recorded_response_creation() {
        let response = RecordedResponse {
            status: 200,
            headers: Vec::new(),
            body: b"test body".to_vec(),
            latency_ms: Some(50),
            timestamp_secs: 1234567890,
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"test body".to_vec());
    }

    #[test]
    fn test_recording_store_record_and_get() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        let sig = RequestSignature::new("GET", "/test", None, &[]);

        let response = RecordedResponse {
            status: 200,
            headers: Vec::new(),
            body: b"response".to_vec(),
            latency_ms: Some(10),
            timestamp_secs: 0,
        };

        store.record(sig.clone(), response);
        let recorded = store.get_recorded(&sig);
        assert!(recorded.is_some());
        assert_eq!(recorded.unwrap().status, 200);
    }

    #[test]
    fn test_recording_store_should_proxy_transparent() {
        let store = RecordingStore::new(ProxyMode::ProxyTransparent);
        let sig = RequestSignature::new("GET", "/test", None, &[]);
        // Transparent mode always proxies
        assert!(store.should_proxy(&sig));
    }

    #[test]
    fn test_recording_store_should_proxy_always() {
        let store = RecordingStore::new(ProxyMode::ProxyAlways);
        let sig = RequestSignature::new("GET", "/test", None, &[]);
        // Always mode always proxies (records but still proxies)
        assert!(store.should_proxy(&sig));

        // Even after recording, it should still proxy
        let response = RecordedResponse {
            status: 200,
            headers: Vec::new(),
            body: vec![],
            latency_ms: None,
            timestamp_secs: 0,
        };
        store.record(sig.clone(), response);
        assert!(store.should_proxy(&sig));
    }

    #[test]
    fn test_recording_store_should_proxy_once() {
        let store = RecordingStore::new(ProxyMode::ProxyOnce);
        let sig = RequestSignature::new("GET", "/unique", None, &[]);

        // First time, should proxy
        assert!(store.should_proxy(&sig));

        // Record a response
        let response = RecordedResponse {
            status: 200,
            headers: Vec::new(),
            body: vec![],
            latency_ms: None,
            timestamp_secs: 0,
        };
        store.record(sig.clone(), response);

        // After recording, should NOT proxy (return cached)
        assert!(!store.should_proxy(&sig));
    }
}
