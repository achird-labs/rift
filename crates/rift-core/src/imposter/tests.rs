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
    };

    let empty_headers = HashMap::new();

    // Should match
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    )); // case-insensitive method

    // Should not match
    assert!(!stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
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
    let imposter = Imposter::new(config);

    let stub = Stub {
        id: None,
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
    };

    let result = imposter.execute_stub_with_rift(&StubState::new(stub));
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
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));

    // Should not match
    assert!(!stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_deep_equals_method() {
    let predicates = vec![serde_json::json!({
        "deepEquals": {"method": "GET"}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    )); // case-insensitive
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_deep_equals_body() {
    let predicates = vec![serde_json::json!({
        "deepEquals": {"body": ""}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Empty body should match
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));

    // Non-empty body should not match
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_contains_query() {
    let predicates = vec![serde_json::json!({
        "contains": {"query": {"lenderIds": "CofTest"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Should match - query contains "CofTest"
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));

    // Should not match
    assert!(!stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_equals_headers() {
    let predicates = vec![serde_json::json!({
        "equals": {"headers": {"Content-Type": "application/json"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());

    assert!(stub_matches(
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
    ));

    // Header key lookup is case-insensitive
    let mut headers_lower = HashMap::new();
    headers_lower.insert("content-type".to_string(), "application/json".to_string());
    assert!(stub_matches(
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
    ));

    // Wrong value
    let mut wrong_headers = HashMap::new();
    wrong_headers.insert("Content-Type".to_string(), "text/html".to_string());
    assert!(!stub_matches(
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
    ));

    // Missing header
    let empty_headers = HashMap::new();
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Missing query param
    assert!(!stub_matches(
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
    ));

    // Missing header
    let empty_headers = HashMap::new();
    assert!(!stub_matches(
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
    ));

    // Missing body
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_logical_not() {
    let predicates = vec![serde_json::json!({
        "not": {"equals": {"method": "DELETE"}}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    // Should match anything except DELETE
    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
    assert!(stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
}

#[test]
fn test_predicate_matches_body_regex() {
    let predicates = vec![serde_json::json!({
        "matches": {"body": "\"userId\":\\s*\"[a-f0-9-]+\""}
    })];
    let predicates = predicates_from_jsons(predicates);

    let empty_headers = HashMap::new();

    assert!(stub_matches(
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
    ));
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Body does NOT have "blah" field → should not match
    assert!(!stub_matches(
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
    ));
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
    assert!(!stub_matches(
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
    ));

    // Body does NOT have "blah" field → should match
    assert!(stub_matches(
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
    ));
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

    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));

    assert!(!stub_matches(
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
    ));
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
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Path is a JSON string but field doesn't end with "123"
    assert!(!stub_matches(
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
    ));
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

    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Body with wrong value
    assert!(!stub_matches(
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
    ));

    // Body missing the field
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));

    assert!(!stub_matches(
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
    ));
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
        ),
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
    assert!(stub_matches(
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
    ));

    // query param 'data' has a field NOT ending in "123"
    assert!(!stub_matches(
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
    ));
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
        ),
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

    assert!(stub_matches(
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
    ));

    // Header value with field NOT ending in "123"
    headers.insert("X-Data".to_string(), r#"{"abc": "other456"}"#.to_string());
    assert!(!stub_matches(
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
    ));
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
        ),
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

    assert!(stub_matches(
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
    ));

    // Form field with value NOT ending in "123"
    form.insert("payload".to_string(), r#"{"key": "other456"}"#.to_string());
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // query param 'data' does NOT contain "ohn"
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Mismatched value
    headers.insert("X-Config".to_string(), r#"{"mode": "prod"}"#.to_string());
    assert!(!stub_matches(
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
    ));
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
        ),
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
    assert!(stub_matches(
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
    ));

    // "Jane" does NOT match ^J.*n$ (ends with 'e')
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // "abc" does NOT match ^\d+$
    headers.insert("X-Data".to_string(), r#"{"id": "abc"}"#.to_string());
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // "abcd" does NOT match ^[A-Z]{3}$
    form.insert("payload".to_string(), r#"{"code": "abcd"}"#.to_string());
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Extra key should be rejected by deepEquals
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Extra key in nested object
    assert!(!stub_matches(
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
    ));
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
    assert!(stub_matches(
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
    ));

    // Different length array
    assert!(!stub_matches(
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
    ));

    // Different values
    assert!(!stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
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

    assert!(!stub_matches(
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
    ));

    assert!(stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
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

    assert!(!stub_matches(
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
    ));

    let mut headers_exact = HashMap::new();
    headers_exact.insert("X-Custom".to_string(), "value".to_string());

    assert!(stub_matches(
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
    ));
}

// =============================================================================
// Issue #87: Header keys should be Title-Case for Mountebank compatibility
// =============================================================================

#[test]
fn test_header_map_to_hashmap_title_case() {
    use hyper::header::HeaderValue;
    use hyper::HeaderMap;

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

    assert!(stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
}

#[test]
fn test_bare_query_param_equals_empty_string() {
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "equals": {
            "query": { "flag": "" }
        }
    })]);

    let empty_headers = HashMap::new();

    assert!(stub_matches(
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
    ));
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

    assert!(stub_matches(
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
    ));
}

#[test]
fn test_multi_valued_query_param_contains() {
    let empty_headers: HashMap<String, String> = HashMap::new();
    let predicates = predicates_from_jsons(vec![serde_json::json!({
        "contains": {
            "query": { "key": "a,b" }
        }
    })]);

    assert!(stub_matches(
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
    ));
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
    let script = "fn should_inject(request, flow_store) { \
         let f = request.headers[\"x-flow-id\"]; if f == () { f = \"MISS\"; }; \
         #{ inject: true, fault: \"error\", status: 200, body: f } }";

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
            flow_state["mountebankStateMapping"] = serde_json::json!({ "flowIdSource": src });
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
            flow_state["mountebankStateMapping"] = serde_json::json!({ "flowIdSource": s });
        }
        let cfg = serde_json::from_value(serde_json::json!({
            "port": port, "protocol": "http",
            "_rift": { "flowState": flow_state }, "stubs": []
        }))
        .unwrap();
        Imposter::new(cfg)
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
        let imp = Imposter::new(cfg);
        let hdrs = hyper::HeaderMap::new();

        // Started ⇒ first eligible (index 0)
        let (_, idx) = imp
            .find_matching_stub("GET", "/s", &hdrs, None, None)
            .expect("match in Started");
        assert_eq!(idx, 0);

        // After paid ⇒ the Started stub is gated out, so index 1 wins
        imp.set_scenario_state("7001", "order", "paid").unwrap();
        let (_, idx) = imp
            .find_matching_stub("GET", "/s", &hdrs, None, None)
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
                "mountebankStateMapping": { "flowIdSource": "header:X-Mock-Space" } } },
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
