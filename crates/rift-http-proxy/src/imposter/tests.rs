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
