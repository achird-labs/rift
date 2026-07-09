//! Tests for the imposter module.
//!
//! This module contains comprehensive tests for:
//! - Imposter configuration serialization/deserialization
//! - Predicate matching (all Mountebank predicates)
//! - Stub execution
//! - ImposterManager lifecycle

use super::*;
use crate::imposter::core::StubState;
use std::collections::HashMap;

fn predicates_from_jsons(predicates: Vec<serde_json::Value>) -> Vec<Predicate> {
    predicates
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect()
}

#[test]
fn test_imposter_config_default() {
    let json = r#"{"port": 8080}"#;
    let config: ImposterConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.port, Some(8080));
    assert_eq!(config.protocol, "http");
    assert!(!config.record_requests);
    assert!(config.stubs.is_empty());
}

#[test]
fn test_imposter_config_no_port() {
    // Port should be optional for auto-assignment
    let json = r#"{"protocol": "http"}"#;
    let config: ImposterConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.port, None);
    assert_eq!(config.protocol, "http");
}

#[test]
fn test_predicate_matching() {
    let stub = Stub {
        id: None,
        route_pattern: None,
        predicates: predicates_from_jsons(vec![serde_json::json!({
            "equals": {
                "method": "GET",
                "path": "/test"
            }
        })]),
        responses: vec![StubResponse::Is {
            is: IsResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Some(serde_json::json!({"message": "hello"})),
                ..Default::default()
            },
            behaviors: None,
            rift: None,
        }],
        scenario_name: None,
        required_scenario_state: None,
        new_scenario_state: None,
        space: None,
        recorded_from: None,
        verify: None,
    };

    let empty_headers = HashMap::new();

    // Should match
    assert!(
        stub_matches(
            &stub.predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &stub.predicates,
            "get",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    ); // case-insensitive method

    // Should not match
    assert!(
        !stub_matches(
            &stub.predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &stub.predicates,
            "GET",
            "/other",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_execute_stub() {
    let config = ImposterConfig {
        port: Some(8080),
        protocol: "http".to_string(),
        name: Some("test".to_string()),
        record_requests: false,
        stubs: vec![],
        default_response: None,
        allow_cors: false,
        service_name: None,
        service_info: None,
        rift: None,
        ..Default::default()
    };
    let imposter = Imposter::new(config).expect("test imposter");

    let stub = Stub {
        id: None,
        route_pattern: None,
        predicates: vec![],
        responses: vec![StubResponse::Is {
            is: IsResponse {
                status_code: 201,
                headers: HashMap::new(),
                body: Some(serde_json::json!({"created": true})),
                ..Default::default()
            },
            behaviors: None,
            rift: None,
        }],
        scenario_name: None,
        required_scenario_state: None,
        new_scenario_state: None,
        space: None,
        recorded_from: None,
        verify: None,
    };

    let result = imposter.execute_stub_with_rift(&StubState::new(stub));
    let result = result.expect("sequencer path is infallible here");
    assert!(result.is_some());
    let (status, _headers, body, _behaviors, _rift_ext, _mode, is_fault) = result.unwrap();
    assert_eq!(status, 201);
    assert!(body.contains("created"));
    assert!(!is_fault);
}

#[test]
fn test_parse_query_string() {
    let query = "name=alice&age=30";
    let parsed = parse_query_string(query);
    assert_eq!(parsed.get("name"), Some(&"alice".to_string()));
    assert_eq!(parsed.get("age"), Some(&"30".to_string()));
}

#[test]
fn test_parse_query_string_url_encoded() {
    // Test URL-encoded values - %2C is a comma, %20 is a space
    let query = "lenderIds=LENDER1%2CLENDER2&name=John%20Doe&path=%2Fapi%2Fusers";
    let parsed = parse_query_string(query);

    // Comma should be decoded
    assert_eq!(
        parsed.get("lenderIds"),
        Some(&"LENDER1,LENDER2".to_string()),
        "URL-encoded comma (%2C) should be decoded"
    );

    // Space should be decoded
    assert_eq!(
        parsed.get("name"),
        Some(&"John Doe".to_string()),
        "URL-encoded space (%20) should be decoded"
    );

    // Forward slashes should be decoded
    assert_eq!(
        parsed.get("path"),
        Some(&"/api/users".to_string()),
        "URL-encoded slashes (%2F) should be decoded"
    );
}

#[test]
fn test_parse_query_string_url_encoded_keys() {
    // Test URL-encoded keys
    let query = "user%20name=alice&filter%5Bstatus%5D=active";
    let parsed = parse_query_string(query);

    assert_eq!(
        parsed.get("user name"),
        Some(&"alice".to_string()),
        "URL-encoded space in key should be decoded"
    );

    assert_eq!(
        parsed.get("filter[status]"),
        Some(&"active".to_string()),
        "URL-encoded brackets in key should be decoded"
    );
}

// Tests for parse_query (used in predicate matching)
#[test]
fn test_parse_query_basic() {
    use crate::imposter::predicates::parse_query;

    let parsed = parse_query(Some("name=alice&age=30"));
    assert_eq!(parsed.get("name"), Some(&"alice".to_string()));
    assert_eq!(parsed.get("age"), Some(&"30".to_string()));

    // None returns empty map
    let empty = parse_query(None);
    assert!(empty.is_empty());
}

#[test]
fn test_parse_query_url_encoded() {
    use crate::imposter::predicates::parse_query;

    // This is the key test - URL-encoded values should be decoded for predicate matching
    let parsed = parse_query(Some("lenderIds=LENDER1%2CLENDER2&name=John%20Doe"));

    assert_eq!(
        parsed.get("lenderIds"),
        Some(&"LENDER1,LENDER2".to_string()),
        "URL-encoded comma (%2C) should be decoded for predicate matching"
    );

    assert_eq!(
        parsed.get("name"),
        Some(&"John Doe".to_string()),
        "URL-encoded space (%20) should be decoded for predicate matching"
    );
}

#[tokio::test]
async fn test_imposter_manager_create_delete() {
    let manager = ImposterManager::new();

    // Try to create an imposter on a high port (less likely to conflict)
    let config = ImposterConfig {
        port: Some(19999),
        protocol: "http".to_string(),
        name: Some("test".to_string()),
        record_requests: false,
        stubs: vec![],
        default_response: None,
        allow_cors: false,
        service_name: None,
        service_info: None,
        rift: None,
        ..Default::default()
    };

    // This may fail if port is in use, which is fine for testing
    let result = manager.create_imposter(config.clone()).await;
    if result.is_ok() {
        assert_eq!(manager.count(), 1);

        // Delete it
        let deleted = manager.delete_imposter(19999).await;
        assert!(deleted.is_ok());
        assert_eq!(manager.count(), 0);
    }
}

#[test]
fn test_add_decorate_behavior_serde() {
    let json = r#"{"to":"http://localhost:4546","mode":"proxyOnce","addDecorateBehavior":"function(request, response) { response.headers['X-Proxied'] = 'true'; }"}"#;

    // Test deserialization
    let proxy: ProxyResponse = serde_json::from_str(json).unwrap();
    assert!(proxy.add_decorate_behavior.is_some());
    assert_eq!(
        proxy.add_decorate_behavior.as_ref().unwrap(),
        "function(request, response) { response.headers['X-Proxied'] = 'true'; }"
    );

    // Test serialization - it should contain addDecorateBehavior
    let serialized = serde_json::to_string(&proxy).unwrap();
    println!("Serialized ProxyResponse: {serialized}");
    assert!(
        serialized.contains("addDecorateBehavior"),
        "Serialized JSON should contain addDecorateBehavior field"
    );
}

#[test]
fn test_imposter_config_with_add_decorate_behavior() {
    let json = r#"{"port": 4545, "protocol": "http", "stubs": [{"responses": [{"proxy": {"to": "http://localhost:4546", "mode": "proxyOnce", "addDecorateBehavior": "function(request, response) { response.headers['X-Proxied'] = 'true'; }"}}]}]}"#;

    // Test deserialization of full imposter config
    let config: ImposterConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.stubs.len(), 1);

    if let StubResponse::Proxy { proxy } = &config.stubs[0].responses[0] {
        println!("Deserialized proxy: {proxy:?}");
        assert!(
            proxy.add_decorate_behavior.is_some(),
            "add_decorate_behavior should be Some after deserialization"
        );
        assert_eq!(
            proxy.add_decorate_behavior.as_ref().unwrap(),
            "function(request, response) { response.headers['X-Proxied'] = 'true'; }"
        );
    } else {
        panic!("Expected Proxy response");
    }

    // Test serialization of full imposter config
    let serialized = serde_json::to_string_pretty(&config).unwrap();
    println!("Serialized ImposterConfig:\n{serialized}");
    assert!(
        serialized.contains("addDecorateBehavior"),
        "Serialized JSON should contain addDecorateBehavior field"
    );
}

#[test]
fn test_alternative_response_format_with_behaviors_array() {
    // Test format with: behaviors array (not _behaviors), statusCode as string, and proxy: null
    let json = r#"{
        "behaviors": [{"wait": 100}],
        "is": {
            "statusCode": "200",
            "headers": {"Content-Type": "application/json"},
            "body": "{\"message\": \"hello\"}"
        },
        "proxy": null
    }"#;

    let response: StubResponse = serde_json::from_str(json).unwrap();
    if let StubResponse::Is { is, behaviors, .. } = response {
        assert_eq!(is.status_code, 200);
        assert!(behaviors.is_some());
        let behaviors = behaviors.unwrap();
        assert_eq!(behaviors.get("wait").unwrap().as_u64(), Some(100));
    } else {
        panic!("Expected Is response");
    }
}

#[test]
fn test_status_code_as_string() {
    let json = r#"{
        "is": {
            "statusCode": "201",
            "headers": {},
            "body": null
        }
    }"#;

    let response: StubResponse = serde_json::from_str(json).unwrap();
    if let StubResponse::Is { is, .. } = response {
        assert_eq!(is.status_code, 201);
    } else {
        panic!("Expected Is response");
    }
}

#[test]
fn test_status_code_as_number() {
    let json = r#"{
        "is": {
            "statusCode": 404,
            "headers": {}
        }
    }"#;

    let response: StubResponse = serde_json::from_str(json).unwrap();
    if let StubResponse::Is { is, .. } = response {
        assert_eq!(is.status_code, 404);
    } else {
        panic!("Expected Is response");
    }
}

#[test]
fn test_behaviors_array_merged_to_object() {
    // Test that behaviors array format is converted to object
    let json = r#"{
        "behaviors": [
            {"wait": 50},
            {"decorate": "function() {}"}
        ],
        "is": {
            "statusCode": 200
        }
    }"#;

    let response: StubResponse = serde_json::from_str(json).unwrap();
    if let StubResponse::Is { behaviors, .. } = response {
        let behaviors = behaviors.expect("behaviors should be present");
        assert!(behaviors.get("wait").is_some());
        assert!(behaviors.get("decorate").is_some());
    } else {
        panic!("Expected Is response");
    }
}

#[test]
fn test_proxy_only_response() {
    // When only proxy is present (not null), it should parse as Proxy variant
    let json = r#"{
        "proxy": {
            "to": "http://example.com",
            "mode": "proxyTransparent"
        }
    }"#;

    let response: StubResponse = serde_json::from_str(json).unwrap();
    if let StubResponse::Proxy { proxy } = response {
        assert_eq!(proxy.to, "http://example.com");
        assert_eq!(proxy.mode, "proxyTransparent");
    } else {
        panic!("Expected Proxy response");
    }
}

#[test]
fn test_full_imposter_config_alternative_format() {
    // Test a complete imposter config with the alternative format
    let json = r#"{
        "port": 8201,
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"equals": {"method": "GET"}}],
                "responses": [
                    {
                        "behaviors": [{"wait": 0}],
                        "is": {
                            "statusCode": "200",
                            "headers": {"Content-Type": "application/json"},
                            "body": "{\"data\": \"test\"}"
                        },
                        "proxy": null
                    }
                ]
            }
        ]
    }"#;

    let config: ImposterConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.port, Some(8201));
    assert_eq!(config.stubs.len(), 1);
    assert_eq!(config.stubs[0].responses.len(), 1);

    if let StubResponse::Is { is, behaviors, .. } = &config.stubs[0].responses[0] {
        assert_eq!(is.status_code, 200);
        assert!(behaviors.is_some());
    } else {
        panic!("Expected Is response");
    }
}

// =============================================================================
// Comprehensive Predicate Tests (Mountebank Compatibility)
// =============================================================================

#[test]
fn test_predicate_ends_with() {
    let predicates = vec![serde_json::json!({
        "endsWith": {"path": "-details"}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Should match
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/api/lender-details",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/user-details",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Should not match
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/details/other",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/api/details/v1",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_deep_equals_method() {
    let predicates = vec![serde_json::json!({
        "deepEquals": {"method": "GET"}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "get",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    ); // case-insensitive
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_deep_equals_body() {
    let predicates = vec![serde_json::json!({
        "deepEquals": {"body": ""}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Empty body should match
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            Some(""),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Non-empty body should not match
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            Some("content"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_contains_query() {
    let predicates = vec![serde_json::json!({
        "contains": {"query": {"lenderIds": "CofTest"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Should match - query contains "CofTest"
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("lenderIds=CofTestWL"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("lenderIds=CofTest"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("lenderIds=123CofTest456"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Should not match
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("lenderIds=Other"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_equals_headers() {
    let predicates = vec![serde_json::json!({
        "equals": {"headers": {"Content-Type": "application/json"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Header key lookup is case-insensitive
    let mut headers_lower = HashMap::new();
    headers_lower.insert("content-type".to_string(), "application/json".to_string());
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers_lower,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Wrong value
    let mut wrong_headers = HashMap::new();
    wrong_headers.insert("Content-Type".to_string(), "text/html".to_string());
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &wrong_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Missing header
    let empty_headers = HashMap::new();
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_exists() {
    let predicates = vec![serde_json::json!({
        "exists": {
            "query": {"token": true},
            "headers": {"Authorization": true},
            "body": true
        }
    })];
    let predicates = predicates_from_jsons(predicates);

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer xyz".to_string());

    // All exist
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            Some("token=abc"),
            &headers,
            Some("body content"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Missing query param
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &headers,
            Some("body content"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Missing header
    let empty_headers = HashMap::new();
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            Some("token=abc"),
            &empty_headers,
            Some("body content"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Missing body
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            Some("token=abc"),
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_logical_not() {
    let predicates = vec![serde_json::json!({
        "not": {"equals": {"method": "DELETE"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Should match anything except DELETE
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "DELETE",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_logical_or() {
    let predicates = vec![serde_json::json!({
        "or": [
            {"equals": {"method": "GET"}},
            {"equals": {"method": "HEAD"}}
        ]
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "HEAD",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_logical_and() {
    let predicates = vec![serde_json::json!({
        "and": [
            {"equals": {"method": "GET"}},
            {"startsWith": {"path": "/api"}}
        ]
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/api/users",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/api/users",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/other",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_matches_regex_all_fields() {
    let predicates = vec![serde_json::json!({
        "matches": {
            "path": "^/api/v[0-9]+/",
            "method": "^(GET|POST)$"
        }
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/api/v1/users",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/api/v2/items",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "DELETE",
            "/api/v1/users",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/other/path",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_predicate_matches_body_regex() {
    let predicates = vec![serde_json::json!({
        "matches": {"body": "\"userId\":\\s*\"[a-f0-9-]+\""}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"userId": "abc-123-def"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"userId": "invalid!"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #75: exists predicate doesn't match inside objects
// =============================================================================

#[test]
fn test_exists_predicate_body_object_field_present() {
    // {"exists": {"body": {"blah": true}}} should check that JSON body contains "blah" field
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "body": {
                "blah": true
            }
        }
    })]);

    let empty_headers = HashMap::new();

    // Body has "blah" field → should match
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"blah": "hello", "other": "stuff"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Body does NOT have "blah" field → should not match
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"other": "stuff"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_body_object_field_absent() {
    // {"exists": {"body": {"blah": false}}} checks that body does NOT contain "blah"
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "body": {
                "blah": false
            }
        }
    })]);

    let empty_headers = HashMap::new();

    // Body has "blah" field → should NOT match (we want it absent)
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"blah": "hello"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Body does NOT have "blah" field → should match
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"other": "stuff"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_body_object_non_json_body() {
    // When body is not valid JSON, object exists predicate should not match for true fields
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "body": {
                "blah": true
            }
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some("not json at all"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_body_boolean_still_works() {
    // Original boolean behavior should still work
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "body": true
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some("any body content"),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #77: Unexpected types in predicate lead to always matching
// =============================================================================

#[test]
fn test_ends_with_object_value_does_not_always_match() {
    // {"endsWith": {"path": {"abc": "123"}}} should NOT match a plain string path
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "path": {"abc": "123"}
        }
    })]);

    let empty_headers = HashMap::new();

    // Path is a plain string, not JSON → should NOT match (was incorrectly always matching)
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/blah",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_ends_with_path_as_json_object() {
    // When path is a JSON string, object predicate should parse it and match recursively
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "path": {"abc": "123"}
        }
    })]);

    let empty_headers = HashMap::new();

    // Path is a JSON string with a field whose value ends with "123"
    assert!(
        stub_matches(
            &predicates,
            "GET",
            r#"{"abc": "other123", "other": "ignored"}"#,
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Path is a JSON string but field doesn't end with "123"
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            r#"{"abc": "other456"}"#,
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_starts_with_object_value_does_not_always_match() {
    // {"startsWith": {"path": {"x": "y"}}} should NOT match a plain string path
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "startsWith": {
            "path": {"x": "y"}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/something",
            None,
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_equals_body_as_json_object() {
    // {"equals": {"body": {"blah": "123"}}} should parse body as JSON and compare fields
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "body": {
                "blah": "123"
            }
        }
    })]);

    let empty_headers = HashMap::new();

    // Body with matching field (extra fields ignored for equals)
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"blah": "123", "other": "ignored"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Body with wrong value
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"blah": "456"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Body missing the field
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"other": "123"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_ends_with_body_object_with_numeric_value() {
    // When expected value is a number in an object, convert to string for comparison
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "body": {"abc": 123}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"abc": "other123"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"abc": "other456"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// Issue #77 (reopened): Object predicates in query, headers, and form
// The original fix only applied check_string_field to method/path/body.
// Query, headers, and form still used inline to_string() which broke
// recursive JSON matching when the expected value is an object.

#[test]
fn test_ends_with_query_object_value_does_not_always_match() {
    // {"endsWith": {"query": {"q": {"nested": "val"}}}} should NOT match
    // when the actual query param value is a plain string
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "query": {"q": {"nested": "val"}}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/search",
            Some("q=hello"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap(),
        "Object expected value in query should not match a plain string"
    );
}

#[test]
fn test_ends_with_query_object_value_recursive_match() {
    // When query param value IS a JSON string, object predicate should recurse
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "query": {"data": {"key": "123"}}
        }
    })]);

    let empty_headers = HashMap::new();

    // query param 'data' is a JSON string with a field ending in "123"
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"key": "other123", "extra": "ignored"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // query param 'data' has a field NOT ending in "123"
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"key": "other456"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_ends_with_header_object_value_does_not_always_match() {
    // {"endsWith": {"headers": {"X-Custom": {"nested": "val"}}}} should NOT match
    // when the actual header value is a plain string
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "headers": {"X-Custom": {"nested": "val"}}
        }
    })]);

    let mut headers = HashMap::new();
    headers.insert("X-Custom".to_string(), "plaintext".to_string());

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap(),
        "Object expected value in headers should not match a plain string"
    );
}

#[test]
fn test_ends_with_header_object_value_recursive_match() {
    // When header value IS a JSON string, object predicate should recurse
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "headers": {"X-Data": {"abc": "123"}}
        }
    })]);

    let mut headers = HashMap::new();
    headers.insert(
        "X-Data".to_string(),
        r#"{"abc": "other123", "extra": "ignored"}"#.to_string(),
    );

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Header value with field NOT ending in "123"
    headers.insert("X-Data".to_string(), r#"{"abc": "other456"}"#.to_string());
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_ends_with_form_object_value_does_not_always_match() {
    // {"endsWith": {"form": {"field": {"nested": "val"}}}} should NOT match
    // when the actual form field value is a plain string
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "form": {"field": {"nested": "val"}}
        }
    })]);

    let empty_headers = HashMap::new();
    let mut form = HashMap::new();
    form.insert("field".to_string(), "plaintext".to_string());

    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/submit",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap(),
        "Object expected value in form should not match a plain string"
    );
}

#[test]
fn test_ends_with_form_object_value_recursive_match() {
    // When form field value IS a JSON string, object predicate should recurse
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "endsWith": {
            "form": {"payload": {"key": "123"}}
        }
    })]);

    let empty_headers = HashMap::new();
    let mut form = HashMap::new();
    form.insert(
        "payload".to_string(),
        r#"{"key": "other123", "extra": "ignored"}"#.to_string(),
    );

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/submit",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap()
    );

    // Form field with value NOT ending in "123"
    form.insert("payload".to_string(), r#"{"key": "other456"}"#.to_string());
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/submit",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap()
    );
}

#[test]
fn test_contains_query_object_value() {
    // Also verify 'contains' operator works for query with object expected values
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "contains": {
            "query": {"data": {"name": "ohn"}}
        }
    })]);

    let empty_headers = HashMap::new();

    // query param 'data' is a JSON string with a field containing "ohn"
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"name": "John", "age": "30"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // query param 'data' does NOT contain "ohn"
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"name": "Jane"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_equals_header_object_value() {
    // 'equals' operator should also recurse for headers with object expected values
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "headers": {"X-Config": {"mode": "test"}}
        }
    })]);

    let mut headers = HashMap::new();
    headers.insert(
        "X-Config".to_string(),
        r#"{"mode": "test", "extra": "ignored"}"#.to_string(),
    );

    // equals with object does recursive field match (extra fields OK)
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Mismatched value
    headers.insert("X-Config".to_string(), r#"{"mode": "prod"}"#.to_string());
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// Issue #77: matches predicate with object expected values in query/headers/form
// The regex function had the same bug — object values were silently skipped via `continue`.

#[test]
fn test_matches_query_object_value_does_not_always_match() {
    // {"matches": {"query": {"q": {"nested": "^abc"}}}} should NOT match
    // when the actual query param is a plain string
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "matches": {
            "query": {"q": {"nested": "^abc"}}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/search",
            Some("q=hello"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap(),
        "Object expected value in matches/query should not match a plain string"
    );
}

#[test]
fn test_matches_query_object_value_recursive_regex() {
    // When query param value IS a JSON string, object predicate should recurse with regex
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "matches": {
            "query": {"data": {"name": "^J.*n$"}}
        }
    })]);

    let empty_headers = HashMap::new();

    // query param 'data' is JSON with a "name" field matching regex ^J.*n$
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"name": "John", "age": "30"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // "Jane" does NOT match ^J.*n$ (ends with 'e')
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            Some(r#"data={"name": "Jane"}"#),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_matches_header_object_value_recursive_regex() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "matches": {
            "headers": {"X-Data": {"id": "^\\d+$"}}
        }
    })]);

    let mut headers = HashMap::new();
    headers.insert(
        "X-Data".to_string(),
        r#"{"id": "12345", "extra": "abc"}"#.to_string(),
    );

    // "12345" matches ^\d+$
    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // "abc" does NOT match ^\d+$
    headers.insert("X-Data".to_string(), r#"{"id": "abc"}"#.to_string());
    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_matches_form_object_value_recursive_regex() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "matches": {
            "form": {"payload": {"code": "^[A-Z]{3}$"}}
        }
    })]);

    let empty_headers = HashMap::new();
    let mut form = HashMap::new();
    form.insert(
        "payload".to_string(),
        r#"{"code": "ABC", "extra": "123"}"#.to_string(),
    );

    // "ABC" matches ^[A-Z]{3}$
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/submit",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap()
    );

    // "abcd" does NOT match ^[A-Z]{3}$
    form.insert("payload".to_string(), r#"{"code": "abcd"}"#.to_string());
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/submit",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #85: deepEquals body missing extra-key check
// =============================================================================

#[test]
fn test_deep_equals_body_extra_keys_rejected() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "deepEquals": {
            "body": {"a": "1"}
        }
    })]);

    let empty_headers = HashMap::new();

    // Exact match should pass
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"a": "1"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Extra key should be rejected by deepEquals
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"a": "1", "b": "2"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_equals_body_extra_keys_allowed() {
    // Regular equals should still allow extra keys
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "body": {"a": "1"}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"a": "1", "b": "2"}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_deep_equals_body_nested_extra_keys_rejected() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "deepEquals": {
            "body": {"outer": {"inner": "val"}}
        }
    })]);

    let empty_headers = HashMap::new();

    // Exact nested match
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"outer": {"inner": "val"}}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Extra key in nested object
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"outer": {"inner": "val", "extra": "x"}}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_deep_equals_body_array_comparison() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "deepEquals": {
            "body": {"items": [1, 2, 3]}
        }
    })]);

    let empty_headers = HashMap::new();

    // Exact array match
    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"items": [1, 2, 3]}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Different length array
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"items": [1, 2, 3, 4]}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    // Different values
    assert!(
        !stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            Some(r#"{"items": [1, 2, 99]}"#),
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #86: keyCaseSensitive missing from check_exists_predicate
// =============================================================================

#[test]
fn test_exists_predicate_query_key_case_insensitive() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "query": {"Token": true}
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("token=abc"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_query_key_case_sensitive() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "query": {"Token": true}
        },
        "caseSensitive": true
    })]);

    let empty_headers = HashMap::new();

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("token=abc"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("Token=abc"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_form_key_case_insensitive() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "form": {"Username": true}
        }
    })]);

    let empty_headers = HashMap::new();
    let mut form = HashMap::new();
    form.insert("username".to_string(), "alice".to_string());

    assert!(
        stub_matches(
            &predicates,
            "POST",
            "/test",
            None,
            &empty_headers,
            None,
            None,
            None,
            Some(&form),
            0
        )
        .unwrap()
    );
}

#[test]
fn test_exists_predicate_headers_key_case_sensitive() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "headers": {"X-Custom": true}
        },
        "caseSensitive": true
    })]);

    let mut headers = HashMap::new();
    headers.insert("x-custom".to_string(), "value".to_string());

    assert!(
        !stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );

    let mut headers_exact = HashMap::new();
    headers_exact.insert("X-Custom".to_string(), "value".to_string());

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers_exact,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #87: Header keys should be Title-Case for Mountebank compatibility
// =============================================================================

#[test]
fn test_header_map_to_hashmap_title_case() {
    use hyper::HeaderMap;
    use hyper::header::HeaderValue;

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("x-custom-header", HeaderValue::from_static("value"));

    let result = Imposter::header_map_to_hashmap(&headers);
    assert!(result.contains_key("Content-Type"));
    assert!(result.contains_key("X-Custom-Header"));
    assert!(!result.contains_key("content-type"));
    assert!(!result.contains_key("x-custom-header"));
}

#[test]
fn test_header_predicate_matches_title_case() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "headers": { "Content-Type": "application/json" }
        }
    })]);

    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            None,
            &headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #84: Bare query params (?flag) dropped
// =============================================================================

#[test]
fn test_parse_query_string_bare_param() {
    let result = parse_query_string("flag");
    assert_eq!(result.get("flag").unwrap(), "");
}

#[test]
fn test_parse_query_string_bare_and_valued() {
    let result = parse_query_string("flag&key=value");
    assert_eq!(result.get("flag").unwrap(), "");
    assert_eq!(result.get("key").unwrap(), "value");
}

#[test]
fn test_bare_query_param_exists_predicate() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "exists": {
            "query": { "flag": true }
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("flag"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_bare_query_param_equals_empty_string() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "query": { "flag": "" }
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("flag"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #83: Multi-valued query params
// =============================================================================

#[test]
fn test_parse_query_string_multi_valued() {
    let result = parse_query_string("key=a&key=b");
    assert_eq!(result.get("key").unwrap(), "a,b");
}

#[test]
fn test_parse_query_string_multi_valued_three() {
    let result = parse_query_string("color=red&color=green&color=blue");
    assert_eq!(result.get("color").unwrap(), "red,green,blue");
}

#[test]
fn test_multi_valued_query_param_equals() {
    let empty_headers: HashMap<String, String> = HashMap::new();
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "query": { "key": "a,b" }
        }
    })]);

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("key=a&key=b"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

#[test]
fn test_multi_valued_query_param_contains() {
    let empty_headers: HashMap<String, String> = HashMap::new();
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "contains": {
            "query": { "key": "a,b" }
        }
    })]);

    assert!(
        stub_matches(
            &predicates,
            "GET",
            "/test",
            Some("key=a&key=b&other=x"),
            &empty_headers,
            None,
            None,
            None,
            None,
            0
        )
        .unwrap()
    );
}

// =============================================================================
// Issue #189: allowCORS — OPTIONS preflight and CORS header injection
// =============================================================================

/// Assert that a response header equals `expected` (`None` asserts absence).
fn assert_cors_header(response: &reqwest::Response, name: &str, expected: Option<&str>) {
    let actual = response.headers().get(name).map(|v| v.to_str().unwrap());
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn test_cors_options_preflight() {
    let manager = ImposterManager::new();
    let config = ImposterConfig {
        port: None,
        protocol: "http".to_string(),
        allow_cors: true,
        stubs: vec![],
        ..Default::default()
    };
    let port = manager
        .create_imposter(config)
        .await
        .expect("failed to create CORS imposter");

    let client = reqwest::Client::new();
    let response = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/any/path"),
        )
        .send()
        .await
        .expect("OPTIONS request failed");

    assert_eq!(response.status(), 200);
    assert_cors_header(&response, "access-control-allow-origin", Some("*"));
    assert_cors_header(&response, "access-control-allow-headers", Some("*"));
    assert_cors_header(&response, "access-control-allow-methods", Some("*"));

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn test_cors_headers_on_stub_response() {
    let manager = ImposterManager::new();
    let stub = Stub {
        id: None,
        route_pattern: None,
        predicates: predicates_from_jsons(vec![serde_json::json!({
            "equals": {"method": "GET", "path": "/test"}
        })]),
        responses: vec![StubResponse::Is {
            is: IsResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Some(serde_json::json!("ok")),
                ..Default::default()
            },
            behaviors: None,
            rift: None,
        }],
        scenario_name: None,
        required_scenario_state: None,
        new_scenario_state: None,
        space: None,
        recorded_from: None,
        verify: None,
    };
    let config = ImposterConfig {
        port: None,
        protocol: "http".to_string(),
        allow_cors: true,
        stubs: vec![stub],
        ..Default::default()
    };
    let port = manager
        .create_imposter(config)
        .await
        .expect("failed to create CORS imposter with stub");

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://127.0.0.1:{port}/test"))
        .send()
        .await
        .expect("GET request failed");

    assert_eq!(response.status(), 200);
    assert_cors_header(&response, "access-control-allow-origin", Some("*"));
    assert_cors_header(&response, "access-control-allow-headers", Some("*"));
    assert_cors_header(&response, "access-control-allow-methods", Some("*"));

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn test_cors_disabled_no_cors_headers() {
    let manager = ImposterManager::new();
    let config = ImposterConfig {
        port: None,
        protocol: "http".to_string(),
        allow_cors: false,
        stubs: vec![],
        ..Default::default()
    };
    let port = manager
        .create_imposter(config)
        .await
        .expect("failed to create imposter without CORS");

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("GET request failed");

    assert_cors_header(&response, "access-control-allow-origin", None);
    assert_cors_header(&response, "access-control-allow-headers", None);
    assert_cors_header(&response, "access-control-allow-methods", None);

    let _ = manager.delete_imposter(port).await;
}

// Issue #213: the `lookup` behavior must apply to direct imposter `is` responses,
// not only to proxied responses. Tokens `${into}[column]` should be replaced from
// the CSV data source.
#[tokio::test]
async fn test_lookup_behavior_applied_on_is_response() {
    let dir = tempfile::tempdir().expect("tempdir");
    let csv_path = dir.path().join("products.csv");
    std::fs::write(&csv_path, "id,name,price\n456,Gadget,19.99\n").expect("write csv");
    let csv_path = csv_path.to_str().expect("csv path utf8");

    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19710,
        "protocol": "http",
        "stubs": [{
            "predicates": [{ "matches": { "path": "^/catalog/\\d+$" } }],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": { "Content-Type": "application/json", "X-Product": "${row}[name]" },
                    "body": { "name": "${row}[name]", "price": "${row}[price]" }
                },
                "_behaviors": {
                    "lookup": {
                        "key": { "from": "path", "using": { "method": "regex", "selector": "/catalog/(\\d+)" } },
                        "fromDataSource": { "csv": { "path": csv_path, "keyColumn": "id" } },
                        "into": "${row}"
                    }
                }
            }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");

    let response = reqwest::Client::new()
        .get("http://127.0.0.1:19710/catalog/456")
        .send()
        .await
        .expect("GET failed");
    let status = response.status();
    let product_header = response
        .headers()
        .get("X-Product")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.expect("body");

    let _ = manager.delete_imposter(19710).await;

    assert_eq!(status, 200, "lookup response should be 200");
    assert!(
        body.contains("Gadget") && body.contains("19.99"),
        "lookup must replace body tokens with CSV values, got: {body}"
    );
    assert!(
        !body.contains("${row}"),
        "no lookup tokens should remain after replacement, got: {body}"
    );
    assert_eq!(
        product_header.as_deref(),
        Some("Gadget"),
        "lookup must replace tokens in response headers too"
    );
}

// Issue #215: scripts must access request headers case-insensitively. A request sent
// with `X-Flow-Id: demo` must be readable as `request.headers["x-flow-id"]` (lowercase),
// matching the engine docs/tests and HTTP's case-insensitive header semantics.
#[tokio::test]
async fn test_script_header_access_is_case_insensitive() {
    let script = "fn respond(ctx) { \
         let f = ctx.request.header(\"x-flow-id\"); if f == () { f = \"MISS\"; }; \
         http(200, f) }";

    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19720,
        "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/whoami" } }],
            "responses": [{ "_rift": { "script": { "engine": "rhai", "code": script } } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");

    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19720/whoami")
        .header("X-Flow-Id", "demo")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");

    let _ = manager.delete_imposter(19720).await;

    assert_eq!(
        body, "demo",
        "script must read the Title-Cased wire header via a lowercase key, got: {body}"
    );
}

// Issue #190: declarative stateful scenarios (whenState/thenState), flow_id-keyed.
#[cfg(test)]
mod scenario_fsm_tests {
    use super::*;

    async fn get(client: &reqwest::Client, port: u16, path: &str, space: Option<&str>) -> String {
        let mut req = client.get(format!("http://127.0.0.1:{port}{path}"));
        if let Some(s) = space {
            req = req.header("X-Mock-Space", s);
        }
        req.send().await.expect("send").text().await.expect("body")
    }

    fn order_fsm(port: u16, flow_id_source: Option<&str>) -> serde_json::Value {
        let mut flow_state = serde_json::json!({ "backend": "inmemory", "ttlSeconds": 300 });
        if let Some(src) = flow_id_source {
            flow_state["flowIdSource"] = serde_json::json!(src);
        }
        serde_json::json!({
            "port": port, "protocol": "http",
            "_rift": { "flowState": flow_state },
            "stubs": [
                { "scenarioName": "order", "requiredScenarioState": "Started",
                  "predicates": [{ "equals": { "path": "/status" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "unpaid" } }] },
                { "scenarioName": "order", "requiredScenarioState": "Started", "newScenarioState": "paid",
                  "predicates": [{ "equals": { "path": "/pay" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] },
                { "scenarioName": "order", "requiredScenarioState": "paid",
                  "predicates": [{ "equals": { "path": "/status" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "paid" } }] }
            ]
        })
    }

    #[tokio::test]
    async fn scenario_auto_provisions_store_without_explicit_flow_state() {
        // No `_rift.flowState` at all: declaring scenario stubs must auto-provision an in-memory
        // store so the FSM works out of the box (otherwise transitions silently no-op on NoOp).
        let manager = ImposterManager::new();
        let config = serde_json::from_value(serde_json::json!({
            "port": 19764, "protocol": "http",
            "stubs": [
                { "scenarioName": "order", "requiredScenarioState": "Started",
                  "predicates": [{ "equals": { "path": "/status" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "unpaid" } }] },
                { "scenarioName": "order", "requiredScenarioState": "Started", "newScenarioState": "paid",
                  "predicates": [{ "equals": { "path": "/pay" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] },
                { "scenarioName": "order", "requiredScenarioState": "paid",
                  "predicates": [{ "equals": { "path": "/status" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "paid" } }] }
            ]
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        assert_eq!(get(&c, 19764, "/status", None).await, "unpaid");
        assert_eq!(get(&c, 19764, "/pay", None).await, "ok");
        assert_eq!(
            get(&c, 19764, "/status", None).await,
            "paid",
            "FSM works without _rift.flowState"
        );

        let _ = manager.delete_imposter(19764).await;
    }

    #[tokio::test]
    async fn scenario_transition_advances_state() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(order_fsm(19760, None)).unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        assert_eq!(
            get(&c, 19760, "/status", None).await,
            "unpaid",
            "initial state"
        );
        assert_eq!(
            get(&c, 19760, "/pay", None).await,
            "ok",
            "pay transitions to paid"
        );
        assert_eq!(
            get(&c, 19760, "/status", None).await,
            "paid",
            "state advanced after pay"
        );

        let _ = manager.delete_imposter(19760).await;
    }

    #[tokio::test]
    async fn scenario_unmatched_in_state_keeps_state() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(order_fsm(19761, None)).unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        // /status only reads; it never carries newScenarioState, so it must not advance.
        assert_eq!(get(&c, 19761, "/status", None).await, "unpaid");
        assert_eq!(
            get(&c, 19761, "/status", None).await,
            "unpaid",
            "read-only stub keeps state"
        );

        let _ = manager.delete_imposter(19761).await;
    }

    #[tokio::test]
    async fn scenario_flow_ids_are_isolated() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(order_fsm(19762, Some("header:X-Mock-Space"))).unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        // Advance only space "alpha"; "beta" stays at the initial state on the same imposter.
        assert_eq!(get(&c, 19762, "/pay", Some("alpha")).await, "ok");
        assert_eq!(
            get(&c, 19762, "/status", Some("alpha")).await,
            "paid",
            "alpha advanced"
        );
        assert_eq!(
            get(&c, 19762, "/status", Some("beta")).await,
            "unpaid",
            "beta isolated"
        );

        let _ = manager.delete_imposter(19762).await;
    }

    fn imposter_with_source(port: u16, src: Option<&str>) -> Imposter {
        let mut flow_state = serde_json::json!({ "backend": "inmemory", "ttlSeconds": 300 });
        if let Some(s) = src {
            flow_state["flowIdSource"] = serde_json::json!(s);
        }
        let cfg = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http",
            "_rift": { "flowState": flow_state }, "stubs": []
        }))
        .unwrap();
        Imposter::new(cfg).expect("test imposter")
    }

    #[test]
    fn resolve_flow_id_modes() {
        // default flow_id_source = imposter_port
        let by_port = imposter_with_source(7000, None);
        assert_eq!(by_port.resolve_flow_id(&HashMap::new()), "7000");

        // header source: present → header value; absent → port fallback
        let by_header = imposter_with_source(7000, Some("header:X-Mock-Space"));
        let mut h = HashMap::new();
        h.insert("X-Mock-Space".to_string(), "abc".to_string());
        assert_eq!(by_header.resolve_flow_id(&h), "abc");
        assert_eq!(by_header.resolve_flow_id(&HashMap::new()), "7000");
    }

    #[test]
    fn scenario_gate_selects_stub_by_state() {
        let cfg = serde_json::from_value(serde_json::json!({
            "port": 7001, "protocol": "http",
            "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300 } },
            "stubs": [
                { "scenarioName": "order", "requiredScenarioState": "Started",
                  "predicates": [{ "equals": { "path": "/s" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "a" } }] },
                { "scenarioName": "order", "requiredScenarioState": "paid",
                  "predicates": [{ "equals": { "path": "/s" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "b" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let hdrs = hyper::HeaderMap::new();

        // Started ⇒ first eligible (index 0)
        let (_, idx) = imp
            .find_matching_stub("GET", "/s", &hdrs, None, None)
            .expect("store is infallible")
            .expect("match in Started");
        assert_eq!(idx, 0);

        // After paid ⇒ the Started stub is gated out, so index 1 wins
        imp.set_scenario_state("7001", "order", "paid").unwrap();
        let (_, idx) = imp
            .find_matching_stub("GET", "/s", &hdrs, None, None)
            .expect("store is infallible")
            .expect("match in paid");
        assert_eq!(idx, 1);
    }

    async fn text(c: &reqwest::Client, url: String) -> String {
        c.get(url)
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("text")
    }

    async fn json(c: &reqwest::Client, url: String) -> serde_json::Value {
        serde_json::from_str(&text(c, url).await).expect("json")
    }
}

// Issue #223: Correlated isolation — space-scoped stubs + one-call per-space teardown.
#[cfg(test)]
mod correlated_space_tests {
    use super::*;

    async fn get(
        c: &reqwest::Client,
        port: u16,
        path: &str,
        space: Option<&str>,
    ) -> reqwest::Response {
        let mut req = c.get(format!("http://127.0.0.1:{port}{path}"));
        if let Some(s) = space {
            req = req.header("X-Mock-Space", s);
        }
        req.send().await.expect("send")
    }

    /// Imposter whose flow_id = the X-Mock-Space header, with the given stubs.
    fn correlated_config(port: u16, stubs: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "port": port, "protocol": "http", "recordRequests": true,
            "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300,
                "flowIdSource": "header:X-Mock-Space" } },
            "stubs": stubs
        })
    }

    #[tokio::test]
    async fn space_scoped_stubs_isolate_responses() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(correlated_config(
            19770,
            serde_json::json!([
                { "space": "alpha", "predicates": [{ "equals": { "path": "/data" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ALPHA" } }] },
                { "space": "beta", "predicates": [{ "equals": { "path": "/data" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "BETA" } }] }
            ]),
        ))
        .unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        assert_eq!(
            get(&c, 19770, "/data", Some("alpha"))
                .await
                .text()
                .await
                .unwrap(),
            "ALPHA"
        );
        assert_eq!(
            get(&c, 19770, "/data", Some("beta"))
                .await
                .text()
                .await
                .unwrap(),
            "BETA"
        );
        // a space with no scoped stub for /data matches neither scoped stub (no leak across spaces)
        let gamma = get(&c, 19770, "/data", Some("gamma"))
            .await
            .text()
            .await
            .unwrap();
        assert!(
            gamma != "ALPHA" && gamma != "BETA",
            "scoped stubs must not leak to gamma, got: {gamma:?}"
        );

        let _ = manager.delete_imposter(19770).await;
    }

    #[tokio::test]
    async fn global_stub_matches_all_spaces() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(correlated_config(
            19771,
            serde_json::json!([
                { "predicates": [{ "equals": { "path": "/health" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "OK" } }] }
            ]),
        ))
        .unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        assert_eq!(
            get(&c, 19771, "/health", Some("alpha"))
                .await
                .text()
                .await
                .unwrap(),
            "OK"
        );
        assert_eq!(
            get(&c, 19771, "/health", Some("beta"))
                .await
                .text()
                .await
                .unwrap(),
            "OK"
        );
        assert_eq!(
            get(&c, 19771, "/health", None).await.text().await.unwrap(),
            "OK"
        );

        let _ = manager.delete_imposter(19771).await;
    }

    #[tokio::test]
    async fn space_scope_composes_with_scenario_fsm() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(correlated_config(19772, serde_json::json!([
            { "space": "alpha", "scenarioName": "order", "requiredScenarioState": "Started",
              "predicates": [{ "equals": { "path": "/status" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "unpaid" } }] },
            { "space": "alpha", "scenarioName": "order", "requiredScenarioState": "Started", "newScenarioState": "paid",
              "predicates": [{ "equals": { "path": "/pay" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] },
            { "space": "alpha", "scenarioName": "order", "requiredScenarioState": "paid",
              "predicates": [{ "equals": { "path": "/status" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "paid" } }] },
            { "space": "beta", "predicates": [{ "equals": { "path": "/status" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "beta" } }] }
        ])))
        .unwrap();
        manager.create_imposter(config).await.expect("create");
        let c = reqwest::Client::new();

        assert_eq!(
            get(&c, 19772, "/status", Some("alpha"))
                .await
                .text()
                .await
                .unwrap(),
            "unpaid"
        );
        assert_eq!(
            get(&c, 19772, "/pay", Some("alpha"))
                .await
                .text()
                .await
                .unwrap(),
            "ok"
        );
        assert_eq!(
            get(&c, 19772, "/status", Some("alpha"))
                .await
                .text()
                .await
                .unwrap(),
            "paid",
            "alpha FSM advanced"
        );
        // beta shares neither stubs nor state with alpha
        assert_eq!(
            get(&c, 19772, "/status", Some("beta"))
                .await
                .text()
                .await
                .unwrap(),
            "beta"
        );

        let _ = manager.delete_imposter(19772).await;
    }
}

// Issue #196: defaultForward — transparently forward unmatched requests upstream.
#[cfg(test)]
mod default_forward_tests {
    use super::*;

    async fn get(port: u16, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("send")
    }

    /// Spin up an upstream imposter that echoes a body per path, on `port`.
    async fn upstream(manager: &ImposterManager, port: u16) {
        let config = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http", "stubs": [
                { "predicates": [{ "equals": { "path": "/ping" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "PONG" } }] },
                { "predicates": [{ "equals": { "path": "/api/v1/users" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "USERS" } }] }
            ]
        }))
        .unwrap();
        manager
            .create_imposter(config)
            .await
            .expect("create upstream");
    }

    #[tokio::test]
    async fn default_forward_proxies_unmatched() {
        let manager = ImposterManager::new();
        upstream(&manager, 19780).await;
        let config = serde_json::from_value(serde_json::json!({
            "port": 19781, "protocol": "http",
            "defaultForward": "http://127.0.0.1:19780", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        let resp = get(19781, "/ping").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-rift-default-forward")
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
        assert_eq!(
            resp.text().await.unwrap(),
            "PONG",
            "unmatched request forwarded upstream"
        );

        let _ = manager.delete_imposter(19780).await;
        let _ = manager.delete_imposter(19781).await;
    }

    #[tokio::test]
    async fn default_forward_preserves_path() {
        let manager = ImposterManager::new();
        upstream(&manager, 19782).await;
        let config = serde_json::from_value(serde_json::json!({
            "port": 19783, "protocol": "http",
            "defaultForward": "http://127.0.0.1:19782", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        // /api/v1/users on the imposter is forwarded to upstream + /api/v1/users
        assert_eq!(
            get(19783, "/api/v1/users").await.text().await.unwrap(),
            "USERS"
        );

        let _ = manager.delete_imposter(19782).await;
        let _ = manager.delete_imposter(19783).await;
    }

    #[tokio::test]
    async fn matching_stub_takes_precedence_over_default_forward() {
        let manager = ImposterManager::new();
        upstream(&manager, 19784).await;
        let config = serde_json::from_value(serde_json::json!({
            "port": 19785, "protocol": "http",
            "defaultForward": "http://127.0.0.1:19784",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/local" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "LOCAL" } }] }
            ]
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        // a matched stub responds locally (not proxied)
        let local = get(19785, "/local").await;
        assert_eq!(local.headers().get("x-rift-default-forward"), None);
        assert_eq!(local.text().await.unwrap(), "LOCAL");
        // an unmatched path still forwards upstream
        assert_eq!(get(19785, "/ping").await.text().await.unwrap(), "PONG");

        let _ = manager.delete_imposter(19784).await;
        let _ = manager.delete_imposter(19785).await;
    }

    #[tokio::test]
    async fn default_forward_upstream_error_returns_502() {
        let manager = ImposterManager::new();
        // defaultForward points at a port with no listener → upstream leg fails
        let config = serde_json::from_value(serde_json::json!({
            "port": 19787, "protocol": "http",
            "defaultForward": "http://127.0.0.1:19999", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        let resp = get(19787, "/anything").await;
        assert_eq!(resp.status(), 502);
        assert_eq!(
            resp.headers()
                .get("x-rift-default-forward-error")
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );

        let _ = manager.delete_imposter(19787).await;
    }

    #[tokio::test]
    async fn default_forward_request_is_audited_when_record_requests_enabled() {
        // The forwarded request still appears in the recordRequests audit log — that feature is
        // independent of the proxy replay cache the transparent forward bypasses.
        let manager = ImposterManager::new();
        upstream(&manager, 19788).await;
        let config = serde_json::from_value(serde_json::json!({
            "port": 19789, "protocol": "http", "recordRequests": true,
            "defaultForward": "http://127.0.0.1:19788", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        assert_eq!(get(19789, "/ping").await.text().await.unwrap(), "PONG");

        let recorded = manager.get_imposter(19789).unwrap().get_recorded_requests();
        assert_eq!(recorded.len(), 1, "forwarded request is audited");
        assert_eq!(recorded[0].path, "/ping");

        let _ = manager.delete_imposter(19788).await;
        let _ = manager.delete_imposter(19789).await;
    }

    #[tokio::test]
    async fn without_default_forward_unmatched_is_unchanged() {
        let manager = ImposterManager::new();
        let config = serde_json::from_value(serde_json::json!({
            "port": 19786, "protocol": "http", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        // existing no-match behaviour: 200 + x-rift-no-match, not a proxied body
        let resp = get(19786, "/anything").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-rift-no-match")
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
        assert!(resp.headers().get("x-rift-default-forward").is_none());

        let _ = manager.delete_imposter(19786).await;
    }
}

// Issue #202: id-addressed stub operations (get/replace/delete by Stub.id), race-free.
#[cfg(test)]
mod id_addressed_stub_tests {
    use super::*;

    fn stub_with_id(id: &str, body: &str) -> Stub {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "predicates": [{ "equals": { "path": format!("/{id}") } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }]
        }))
        .unwrap()
    }

    async fn imposter_on(port: u16) -> std::sync::Arc<ImposterManager> {
        let manager = std::sync::Arc::new(ImposterManager::new());
        let config = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http", "stubs": []
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");
        manager
    }

    #[tokio::test]
    async fn delete_stub_by_id_preserves_position() {
        let manager = imposter_on(19810).await;
        for id in ["a", "b", "c"] {
            manager
                .add_stub(19810, stub_with_id(id, id), None)
                .await
                .unwrap();
        }
        manager.delete_stub_by_id(19810, "b").await.unwrap();

        let stubs = manager.get_imposter(19810).unwrap().get_stubs();
        let ids: Vec<_> = stubs.iter().map(|s| s.id.clone().unwrap()).collect();
        assert_eq!(ids, vec!["a", "c"], "b removed; a/c keep relative order");
        assert!(matches!(
            manager.get_stub_by_id(19810, "b"),
            Err(ImposterError::StubNotFound(_))
        ));
        let _ = manager.delete_imposter(19810).await;
    }

    #[tokio::test]
    async fn duplicate_stub_id_conflicts() {
        let manager = imposter_on(19811).await;
        manager
            .add_stub(19811, stub_with_id("x", "first"), None)
            .await
            .unwrap();
        let dup = manager
            .add_stub(19811, stub_with_id("x", "second"), None)
            .await;
        assert!(
            matches!(dup, Err(ImposterError::StubIdConflict(_))),
            "duplicate id rejected"
        );
        // the original is untouched
        assert_eq!(
            manager.get_stub_by_id(19811, "x").unwrap().responses.len(),
            1
        );
        let _ = manager.delete_imposter(19811).await;
    }

    #[tokio::test]
    async fn replace_stub_by_id_in_place() {
        let manager = imposter_on(19812).await;
        for id in ["a", "b", "c"] {
            manager
                .add_stub(19812, stub_with_id(id, id), None)
                .await
                .unwrap();
        }
        // replace b's content; a PUT body whose id differs must not change the addressable id
        let mut replacement = stub_with_id("ignored", "B2");
        replacement.id = Some("ignored".into());
        manager
            .replace_stub_by_id(19812, "b", replacement)
            .await
            .unwrap();

        let stubs = manager.get_imposter(19812).unwrap().get_stubs();
        let ids: Vec<_> = stubs.iter().map(|s| s.id.clone().unwrap()).collect();
        assert_eq!(
            ids,
            vec!["a", "b", "c"],
            "position + addressable id preserved"
        );
        // content updated, still addressable by the path id
        let got = manager.get_stub_by_id(19812, "b").unwrap();
        assert_eq!(got.id.as_deref(), Some("b"));
        let body = serde_json::to_value(&got).unwrap();
        assert_eq!(
            body["responses"][0]["is"]["body"], "B2",
            "content actually replaced"
        );

        assert!(matches!(
            manager
                .replace_stub_by_id(19812, "missing", stub_with_id("missing", "z"))
                .await,
            Err(ImposterError::StubNotFound(_))
        ));
        let _ = manager.delete_imposter(19812).await;
    }

    #[tokio::test]
    async fn concurrent_same_id_add_exactly_one_wins() {
        let manager = imposter_on(19814).await;
        // 12 tasks race to add the SAME id — the atomic dedup-under-lock must admit exactly one.
        let mut adds = tokio::task::JoinSet::new();
        for _ in 0..12 {
            let m = manager.clone();
            adds.spawn(async move { m.add_stub(19814, stub_with_id("dup", "v"), None).await });
        }
        let mut wins = 0;
        let mut conflicts = 0;
        while let Some(r) = adds.join_next().await {
            match r.unwrap() {
                Ok(()) => wins += 1,
                Err(ImposterError::StubIdConflict(_)) => conflicts += 1,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(wins, 1, "exactly one add succeeds");
        assert_eq!(conflicts, 11, "the rest are conflicts");
        assert_eq!(manager.get_imposter(19814).unwrap().get_stubs().len(), 1);
        let _ = manager.delete_imposter(19814).await;
    }

    #[tokio::test]
    async fn concurrent_id_ops_stay_consistent() {
        let manager = imposter_on(19813).await;
        // add 0..20 concurrently, then delete the even ids concurrently
        let mut adds = tokio::task::JoinSet::new();
        for i in 0..20u32 {
            let m = manager.clone();
            adds.spawn(async move {
                m.add_stub(19813, stub_with_id(&i.to_string(), "v"), None)
                    .await
            });
        }
        while let Some(r) = adds.join_next().await {
            r.unwrap().unwrap();
        }
        let mut dels = tokio::task::JoinSet::new();
        for i in (0..20u32).step_by(2) {
            let m = manager.clone();
            dels.spawn(async move { m.delete_stub_by_id(19813, &i.to_string()).await });
        }
        while let Some(r) = dels.join_next().await {
            r.unwrap().unwrap();
        }

        let stubs = manager.get_imposter(19813).unwrap().get_stubs();
        assert_eq!(
            stubs.len(),
            10,
            "20 added, 10 deleted → 10 remain, no lost/dup"
        );
        for i in 0..20u32 {
            let present = manager.get_stub_by_id(19813, &i.to_string()).is_ok();
            assert_eq!(present, i % 2 == 1, "only odd ids survive ({i})");
        }
        let _ = manager.delete_imposter(19813).await;
    }
}

// =========================================================================
// Issue #318: backend error propagation + per-request annotations + decorator
// =========================================================================
mod backend_errors {
    use super::*;
    use crate::extensions::decorate::{
        BackendUnavailable, ResponseDecorator, ResponsePhase, annotate,
    };
    use crate::extensions::flow_state::FlowStore;
    use crate::imposter::core::Imposter;
    use crate::imposter::handler::handle_imposter_request_decorated;
    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use std::sync::Arc;

    /// A backend whose reads and writes fail with `BackendUnavailable`, annotating each op.
    struct FailingStore;

    impl FlowStore for FailingStore {
        fn get(&self, _flow_id: &str, _key: &str) -> anyhow::Result<Option<Value>> {
            annotate("flowStore.get", "induced-failure".to_string());
            Err(anyhow::Error::new(BackendUnavailable {
                feature: "flowState",
                detail: "induced get failure".to_string(),
            }))
        }
        fn set(&self, _flow_id: &str, _key: &str, _value: Value) -> anyhow::Result<()> {
            annotate("flowStore.set", "induced-failure".to_string());
            Err(anyhow::Error::new(BackendUnavailable {
                feature: "flowState",
                detail: "induced set failure".to_string(),
            }))
        }
        fn exists(&self, _flow_id: &str, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn delete(&self, _flow_id: &str, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn increment(&self, _flow_id: &str, _key: &str) -> anyhow::Result<i64> {
            Ok(1)
        }
        fn set_ttl(&self, _flow_id: &str, _ttl_seconds: i64) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn gated_imposter() -> Arc<Imposter> {
        let config: ImposterConfig = serde_json::from_value(json!({
            "protocol": "http", "port": 19490,
            "stubs": [{
                "scenarioName": "order",
                "requiredScenarioState": "Started",
                "predicates": [{"equals": {"path": "/gated"}}],
                "responses": [{"is": {"statusCode": 200, "body": "ok"}}]
            }]
        }))
        .expect("config");
        let mut imposter = Imposter::new(config).expect("test imposter");
        imposter.flow_store = Arc::new(FailingStore);
        Arc::new(imposter)
    }

    // AC3 (propagation): the scenario eligibility gate no longer swallows store errors.
    #[test]
    fn scenario_gate_propagates_store_error() {
        let imposter = gated_imposter();
        let err = imposter
            .find_matching_stub("GET", "/gated", &hyper::HeaderMap::new(), None, None)
            .expect_err("store failure must propagate, not default to initial state");
        assert!(
            err.downcast_ref::<BackendUnavailable>().is_some(),
            "typed error must survive: {err:?}"
        );
    }

    // A non-string value stored on a scenario key (out-of-band raw flow-state PUT) is an
    // error, never silently coerced to the initial state.
    #[test]
    fn scenario_state_non_string_value_errors() {
        let config: ImposterConfig = serde_json::from_value(json!({
            "protocol": "http", "port": 19492,
            "stubs": [{
                "scenarioName": "order",
                "requiredScenarioState": "Started",
                "predicates": [{"equals": {"path": "/gated"}}],
                "responses": [{"is": {"statusCode": 200}}]
            }]
        }))
        .expect("config");
        let imposter = Imposter::new(config).expect("test imposter");
        imposter
            .flow_set("f", "order", json!(42))
            .expect("in-memory set");
        let err = imposter
            .scenario_state("f", "order")
            .expect_err("non-string state must error");
        assert!(err.to_string().contains("not a string"), "got: {err}");
    }

    // AC3 (propagation): a failed newScenarioState write propagates too.
    #[test]
    fn scenario_transition_error_propagates() {
        let imposter = gated_imposter();
        let stub: Stub = serde_json::from_value(json!({
            "scenarioName": "order",
            "newScenarioState": "paid",
            "predicates": [],
            "responses": [{"is": {"statusCode": 200}}]
        }))
        .expect("stub");
        let err = imposter
            .apply_scenario_transition("flow", &stub)
            .expect_err("transition write failure must propagate");
        assert!(err.downcast_ref::<BackendUnavailable>().is_some());
    }

    type DecoratorCall = (ResponsePhase, Option<u16>, Vec<(&'static str, String)>);

    #[derive(Default)]
    struct RecordingDecorator {
        calls: Mutex<Vec<DecoratorCall>>,
    }

    impl ResponseDecorator for RecordingDecorator {
        fn decorate(
            &self,
            phase: ResponsePhase,
            req_port: Option<u16>,
            annotations: &[(&'static str, String)],
            headers: &mut hyper::HeaderMap,
        ) {
            self.calls
                .lock()
                .push((phase, req_port, annotations.to_vec()));
            headers.insert("x-test-decorated", "1".parse().expect("header"));
        }
    }

    // AC2 + AC3 end-to-end on the data plane: a bare listener serving the decorated
    // entrypoint returns the structured 503, and the decorator sees the phase, the
    // imposter port, and the annotations the failing backend attached.
    #[tokio::test]
    async fn decorated_data_plane_serves_503_and_annotations() {
        use hyper::server::conn::http1;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;

        let imposter = gated_imposter();
        let recorder = Arc::new(RecordingDecorator::default());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:12617")
            .await
            .expect("bind");
        let imp = imposter.clone();
        let rec = recorder.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, addr)) = listener.accept().await else {
                    break;
                };
                let imp = imp.clone();
                let rec = rec.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let imp = imp.clone();
                        let rec = rec.clone();
                        async move {
                            handle_imposter_request_decorated(
                                req,
                                imp,
                                addr,
                                19490,
                                Some(rec as Arc<dyn ResponseDecorator>),
                            )
                            .await
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let resp = reqwest::get("http://127.0.0.1:12617/gated")
            .await
            .expect("request");
        assert_eq!(resp.status(), 503, "backend failure is a structured 503");
        assert_eq!(
            resp.headers()
                .get("x-test-decorated")
                .and_then(|v| v.to_str().ok()),
            Some("1"),
            "decorator header must land on the wire"
        );
        let body: serde_json::Value = resp.json().await.expect("json");
        assert_eq!(body["error"], "backendUnavailable");
        assert_eq!(body["feature"], "flowState");

        let calls = recorder.calls.lock().clone();
        assert_eq!(calls.len(), 1);
        let (phase, port, annotations) = &calls[0];
        assert_eq!(*phase, ResponsePhase::DataPlane);
        assert_eq!(*port, Some(19490));
        assert!(
            annotations
                .iter()
                .any(|(k, v)| *k == "flowStore.get" && v == "induced-failure"),
            "backend annotation must reach the decorator: {annotations:?}"
        );
    }

    // AC2: the manager's builder wires the decorator into the real serve loop.
    #[tokio::test]
    async fn manager_with_decorator_decorates_normal_response() {
        let recorder = Arc::new(RecordingDecorator::default());
        let manager = ImposterManager::new()
            .with_response_decorator(recorder.clone() as Arc<dyn ResponseDecorator>);
        let config: ImposterConfig = serde_json::from_value(json!({
            "protocol": "http", "port": 19491,
            "stubs": [{
                "predicates": [{"equals": {"path": "/ping"}}],
                "responses": [{"is": {"statusCode": 200, "body": "pong"}}]
            }]
        }))
        .expect("config");
        manager.create_imposter(config).await.expect("create");

        let resp = reqwest::get("http://127.0.0.1:19491/ping")
            .await
            .expect("request");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-test-decorated")
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );

        let calls = recorder.calls.lock().clone();
        assert_eq!(calls.len(), 1);
        let (phase, port, annotations) = &calls[0];
        assert_eq!(*phase, ResponsePhase::DataPlane);
        assert_eq!(*port, Some(19491));
        assert!(annotations.is_empty(), "plain stub produces no annotations");

        manager.delete_all().await;
    }
}

// =========================================================================
// Issue #311: scenario transitions are compare-and-set, not get-then-set
// =========================================================================
mod cas_transitions {
    use super::*;
    use crate::imposter::core::Imposter;
    use serde_json::json;

    fn imposter_with(stub: serde_json::Value) -> Imposter {
        let config: ImposterConfig = serde_json::from_value(json!({
            "protocol": "http", "port": 19506,
            "stubs": [stub]
        }))
        .expect("config");
        Imposter::new(config).expect("test imposter")
    }

    fn gated_stub() -> serde_json::Value {
        json!({
            "scenarioName": "order",
            "requiredScenarioState": "Started",
            "newScenarioState": "paid",
            "predicates": [{"equals": {"path": "/pay"}}],
            "responses": [{"is": {"statusCode": 200}}]
        })
    }

    // The deterministic red for the lost-update class: a transition whose gate state has
    // moved underneath must NOT clobber the newer state (old code overwrote it).
    #[test]
    fn transition_conflict_does_not_clobber() {
        let imposter = imposter_with(gated_stub());
        let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
        imposter
            .set_scenario_state("f", "order", "shipped")
            .expect("seed moved state");

        imposter
            .apply_scenario_transition("f", &stub)
            .expect("conflict is not an error");
        assert_eq!(
            imposter.scenario_state("f", "order").expect("state"),
            "shipped",
            "a stale transition must not overwrite a state that moved underneath"
        );
    }

    #[test]
    fn transition_from_stored_initial_state_applies() {
        let imposter = imposter_with(gated_stub());
        let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
        imposter
            .set_scenario_state("f", "order", "Started")
            .expect("seed explicit initial");

        imposter
            .apply_scenario_transition("f", &stub)
            .expect("transition");
        assert_eq!(
            imposter.scenario_state("f", "order").expect("state"),
            "paid"
        );
    }

    #[test]
    fn transition_from_absent_initial_applies() {
        let imposter = imposter_with(gated_stub());
        let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");

        imposter
            .apply_scenario_transition("f", &stub)
            .expect("transition");
        assert_eq!(
            imposter.scenario_state("f", "order").expect("state"),
            "paid",
            "initial state stored as absence must satisfy the gate expectation"
        );
    }

    // The retry-then-lose sub-arm: a racer writes between the two CAS calls of the
    // absent-initial path. Losing must be a silent no-write Ok, never an error or a set.
    #[test]
    fn losing_the_initial_state_retry_is_a_silent_no_write() {
        use crate::extensions::flow_state::{CasOutcome, FlowStore};
        use serde_json::Value;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct RetryLoserStore {
            writes: AtomicUsize,
        }

        impl FlowStore for RetryLoserStore {
            fn get(&self, _f: &str, _k: &str) -> anyhow::Result<Option<Value>> {
                Ok(None)
            }
            fn set(&self, _f: &str, _k: &str, _v: Value) -> anyhow::Result<()> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn exists(&self, _f: &str, _k: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn delete(&self, _f: &str, _k: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn increment(&self, _f: &str, _k: &str) -> anyhow::Result<i64> {
                Ok(1)
            }
            fn set_ttl(&self, _f: &str, _t: i64) -> anyhow::Result<()> {
                Ok(())
            }
            fn compare_and_set(
                &self,
                _f: &str,
                _k: &str,
                expected: Option<&Value>,
                _new: Value,
            ) -> anyhow::Result<CasOutcome> {
                match expected {
                    // First CAS (expecting "Started"): key looks absent.
                    Some(_) => Ok(CasOutcome::Conflict(None)),
                    // Retry (expecting absent): a racer won in between.
                    None => Ok(CasOutcome::Conflict(Some(serde_json::json!(
                        "paid-elsewhere"
                    )))),
                }
            }
        }

        let mut imposter = imposter_with(gated_stub());
        let store = std::sync::Arc::new(RetryLoserStore::default());
        imposter.flow_store = store.clone();
        let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");

        imposter
            .apply_scenario_transition("f", &stub)
            .expect("losing the retry is not an error");
        assert_eq!(
            store.writes.load(Ordering::SeqCst),
            0,
            "the losing retry must not write"
        );
    }

    #[test]
    fn ungated_transition_still_unconditional() {
        let stub_json = json!({
            "scenarioName": "order",
            "newScenarioState": "paid",
            "predicates": [{"equals": {"path": "/pay"}}],
            "responses": [{"is": {"statusCode": 200}}]
        });
        let imposter = imposter_with(stub_json.clone());
        let stub: Stub = serde_json::from_value(stub_json).expect("stub");
        imposter
            .set_scenario_state("f", "order", "shipped")
            .expect("seed");

        imposter
            .apply_scenario_transition("f", &stub)
            .expect("transition");
        assert_eq!(
            imposter.scenario_state("f", "order").expect("state"),
            "paid",
            "an ungated transition keeps today's unconditional overwrite semantics"
        );
    }
}

// Issue #433: a stub-level `routePattern` populates `request.pathParams.<name>` for both response
// templates and every script engine; absent a pattern the map stays empty (unchanged default).
#[tokio::test]
async fn test_path_params_template_end_to_end() {
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19741,
        "protocol": "http",
        "stubs": [{
            "routePattern": "/users/:id",
            "predicates": [{ "equals": { "path": "/users/123" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "${request.pathParams.id}" } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19741/users/123")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(19741).await;

    assert_eq!(
        body, "123",
        "routePattern must populate ${{request.pathParams.id}} in the response template, got: {body}"
    );
}

#[tokio::test]
async fn test_path_params_script_rhai() {
    let script = "fn respond(ctx) { \
         let id = ctx.request.pathParams[\"id\"]; if id == () { id = \"MISS\"; } \
         http(200, id) }";
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19742,
        "protocol": "http",
        "stubs": [{
            "routePattern": "/users/:id",
            "predicates": [{ "equals": { "path": "/users/123" } }],
            "responses": [{ "_rift": { "script": { "engine": "rhai", "code": script } } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19742/users/123")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(19742).await;

    assert_eq!(
        body, "123",
        "rhai script must read a populated request.pathParams.id, got: {body}"
    );
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn test_path_params_script_js() {
    let script = "function respond(ctx) { \
         return http(200, ctx.request.pathParams.id); }";
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19744,
        "protocol": "http",
        "stubs": [{
            "routePattern": "/users/:id",
            "predicates": [{ "equals": { "path": "/users/123" } }],
            "responses": [{ "_rift": { "script": { "engine": "javascript", "code": script } } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19744/users/123")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(19744).await;

    assert_eq!(
        body, "123",
        "js script must read a populated request.pathParams.id, got: {body}"
    );
}

#[tokio::test]
async fn test_path_params_absent_pattern_is_empty() {
    // No routePattern → pathParams stays empty and nothing errors (unchanged default). The `[]`
    // wrapper makes the empty substitution observable.
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19745,
        "protocol": "http",
        "stubs": [{
            "predicates": [{ "equals": { "path": "/users/456" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "[${request.pathParams.id}]" } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19745/users/456")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(19745).await;

    assert_eq!(
        body, "[]",
        "without routePattern, request.pathParams.id resolves to empty, got: {body}"
    );
}

#[tokio::test]
async fn test_path_params_non_matching_pattern_is_empty() {
    // routePattern is set but structurally does not match the request path (segment counts differ):
    // the predicate still matches, so the stub is served, but pathParams must be empty (no error).
    let config: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19746,
        "protocol": "http",
        "stubs": [{
            "routePattern": "/users/:id",
            "predicates": [{ "equals": { "path": "/users/123/profile" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "[${request.pathParams.id}]" } }]
        }]
    }))
    .expect("config");

    let manager = ImposterManager::new();
    manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    let body = reqwest::Client::new()
        .get("http://127.0.0.1:19746/users/123/profile")
        .send()
        .await
        .expect("GET failed")
        .text()
        .await
        .expect("body");
    let _ = manager.delete_imposter(19746).await;

    assert_eq!(
        body, "[]",
        "a routePattern that doesn't match the path shape yields empty pathParams, got: {body}"
    );
}

// Issue #357 B2: a `_rift.script` stub can dynamically call `reset()` (the v2 connection-reset
// result constructor), so `uses_tcp_faults()` must report true for it — forcing the imposter
// HTTP/1-only, since a socket reset is incompatible with HTTP/2 stream multiplexing.
#[test]
fn script_bearing_stub_forces_http1_only() {
    use crate::imposter::core::Imposter;

    let with_script: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19507,
        "protocol": "http",
        "stubs": [{
            "responses": [{ "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { reset() }" } } }]
        }]
    }))
    .expect("config");
    let imposter = Imposter::new(with_script).expect("test imposter");
    assert!(
        imposter.uses_tcp_faults(),
        "a stub carrying a _rift.script (which may call reset()) must force HTTP/1-only"
    );

    // Control: a plain `is` stub with no script and no tcp fault stays HTTP/2-eligible.
    let plain: ImposterConfig = serde_json::from_value(serde_json::json!({
        "port": 19508,
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
    }))
    .expect("config");
    let plain_imposter = Imposter::new(plain).expect("test imposter");
    assert!(
        !plain_imposter.uses_tcp_faults(),
        "a plain is-response stub must not be forced HTTP/1-only"
    );
}
