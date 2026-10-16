//! Mountebank compatibility integration tests.
//!
//! These tests verify that Rift behaves compatibly with Mountebank's HTTP imposter API.
//! Tests are organized by feature category and require a running Rift server.

mod support;

use reqwest::Client;
use serde_json::json;
use std::time::Duration;

const ADMIN_URL: &str = "http://127.0.0.1";
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Start a Rift server for testing, returning it with the admin port it bound.
async fn start_rift_server() -> (tokio::process::Child, u16) {
    support::spawn_server(&["--allow-injection"]).await
}

/// Create an imposter via the admin API
async fn create_imposter(client: &Client, admin_port: u16, config: serde_json::Value) -> u16 {
    let response = client
        .post(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .json(&config)
        .send()
        .await
        .expect("Failed to create imposter");

    assert!(
        response.status().is_success(),
        "Failed to create imposter: {}",
        response.text().await.unwrap_or_default()
    );

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    body["port"].as_u64().expect("Missing port in response") as u16
}

/// Delete all imposters
async fn clear_imposters(client: &Client, admin_port: u16) {
    let _ = client
        .delete(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .send()
        .await;
}

// =============================================================================
// 1. Basic Predicates
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_equals_method_path() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"equals": {"method": "POST", "path": "/api/data"}}],
                "responses": [{"is": {"statusCode": 201, "body": "created"}}]
            },
            {
                "predicates": [{"equals": {"method": "GET", "path": "/api/data"}}],
                "responses": [{"is": {"statusCode": 200, "body": "data"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // GET should match second stub
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/data"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "data");

    // POST should match first stub
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/api/data"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.text().await.unwrap(), "created");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_equals_query_parameter() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"query": {"status": "active"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "active users"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/users?status=active"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "active users");

    // Non-matching query
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/users?status=inactive"))
        .send()
        .await
        .unwrap();
    // Should not match, get default response
    assert_ne!(resp.text().await.unwrap(), "active users");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_equals_header_default_case_insensitive() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"headers": {"X-Custom": "test-value"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "header matched"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Headers are case-insensitive by default
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .header("x-custom", "test-value")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "header matched");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_contains_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"contains": {"body": "search-term"}}],
            "responses": [{"is": {"statusCode": 200, "body": "found"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/search"))
        .body("this contains search-term in it")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "found");

    // Non-matching body
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/search"))
        .body("this does not contain it")
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "found");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_starts_with_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"startsWith": {"path": "/api/"}}],
            "responses": [{"is": {"statusCode": 200, "body": "api response"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "api response");

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/web/page"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "api response");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_ends_with_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"endsWith": {"path": ".json"}}],
            "responses": [{"is": {"statusCode": 200, "body": "json endpoint"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/data/file.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "json endpoint");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_matches_regex_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"matches": {"path": "^/api/users/\\d+$"}}],
            "responses": [{"is": {"statusCode": 200, "body": "user found"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users/123"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "user found");

    // Non-numeric ID - should not match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users/abc"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "user found");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_case_sensitive_true() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"body": "Hello World"},
                "caseSensitive": true
            }],
            "responses": [{"is": {"statusCode": 200, "body": "matched"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Exact case match
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/"))
        .body("Hello World")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "matched");

    // Different case - should NOT match with caseSensitive: true
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/"))
        .body("hello world")
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "matched");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_except_regex_exclusion() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"path": "/users"},
                "except": "^/api"
            }],
            "responses": [{"is": {"statusCode": 200, "body": "matched with except"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // /api/users -> except removes "/api" -> "/users" matches "/users"
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "matched with except");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 2. deepEquals Semantics
// =============================================================================

/// deepEquals body does structural comparison (Issue #85 - fixed)
#[tokio::test]
#[ignore = "requires running server"]
async fn test_deep_equals_body_json_key_order() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "deepEquals": {"body": {"b": 2, "a": 1}}
            }],
            "responses": [{"is": {"statusCode": 200, "body": "deep match"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Body with keys in different order should match structurally
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/"))
        .header("content-type", "application/json")
        .body(r#"{"a":1,"b":2}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "deepEquals body should match regardless of JSON key order"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_deep_equals_query_extra_params() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"deepEquals": {"query": {"a": "1"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "exact query"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Exact match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/?a=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "exact query");

    // Extra param - deepEquals should fail
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/?a=1&b=2"))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.text().await.unwrap(),
        "exact query",
        "deepEquals query should fail when extra params present"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_deep_equals_headers_extra() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Note: deepEquals on headers is tricky because HTTP clients add default headers
    // This test verifies the concept but may need adjustment for real header counts
    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"deepEquals": {"query": {"only": "this"}}}],
                "responses": [{"is": {"statusCode": 200, "body": "deep match"}}]
            },
            {
                "predicates": [],
                "responses": [{"is": {"statusCode": 404, "body": "no match"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Extra query param should cause deepEquals to fail
    let resp = client
        .get(format!(
            "{ADMIN_URL}:{imposter_port}/?only=this&extra=param"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.unwrap(), "no match");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 3. Query String Edge Cases
// =============================================================================

/// Multi-valued query parameters are comma-joined (Issue #83 - fixed)
#[tokio::test]
#[ignore = "requires running server"]
async fn test_query_multi_valued_params() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Multi-valued params are comma-joined: ?key=first&key=second → "first,second"
    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"query": {"key": "first,second"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "found both"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!(
            "{ADMIN_URL}:{imposter_port}/search?key=first&key=second"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "Multi-valued query params should be comma-joined and matchable"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

/// Bare query params without '=' are treated as key="" (Issue #84 - fixed)
#[tokio::test]
#[ignore = "requires running server"]
async fn test_query_bare_param_no_equals() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"exists": {"query": {"flag": true}}}],
            "responses": [{"is": {"statusCode": 200, "body": "flag exists"}}]
        }, {
            "predicates": [{"equals": {"query": {"flag": ""}}}],
            "responses": [{"is": {"statusCode": 200, "body": "flag is empty"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/search?flag"))
        .send()
        .await
        .unwrap();

    // `?flag` parses to flag='' (#84), and Mountebank's `exists` reads '' as absent (#1403).
    assert_eq!(
        resp.text().await.unwrap(),
        "flag is empty",
        "Bare query param '?flag' should be treated as flag='', which exists reads as absent"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_query_mixed_bare_and_valued() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"query": {"a": "1"}},
            }, {
                "exists": {"query": {"flag": true}}
            }],
            "responses": [{"is": {"statusCode": 200, "body": "flag exists"}}]
        }, {
            "predicates": [{
                "equals": {"query": {"a": "1", "flag": "", "b": "2"}},
            }],
            "responses": [{"is": {"statusCode": 200, "body": "mixed match"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Bug 2: ?a=1&flag&b=2 - "flag" is dropped by parse_query_string
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/?a=1&flag&b=2"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.text().await.unwrap(),
        "mixed match",
        "Bug 2: the bare param survives parsing as flag='' (and exists reads it as absent, #1403)"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_query_url_encoded_params() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Already fixed in #70 - verify through integration test
    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"query": {"name": "hello world"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "decoded match"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!(
            "{ADMIN_URL}:{imposter_port}/test?name=hello%20world"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "decoded match");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_query_empty_string() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"query": {"key": ""}}}],
            "responses": [{"is": {"statusCode": 200, "body": "empty value"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test?key="))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "empty value");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 4. Header Edge Cases
// =============================================================================

/// Header keys are Title-Case for keyCaseSensitive matching (Issue #87 - fixed)
#[tokio::test]
#[ignore = "requires running server"]
async fn test_header_key_case_sensitive_true() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"headers": {"Content-Type": "application/json"}},
                "keyCaseSensitive": true
            }],
            "responses": [{"is": {"statusCode": 200, "body": "key sensitive match"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .header("Content-Type", "application/json")
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "keyCaseSensitive=true with Title-Case header should match"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_header_value_case_sensitivity() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"headers": {"x-custom": "TestValue"}},
                "caseSensitive": true
            }],
            "responses": [{"is": {"statusCode": 200, "body": "case match"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Exact case match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .header("x-custom", "TestValue")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Different case - should NOT match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .header("x-custom", "testvalue")
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "case match");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 5. exists Predicate
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_exists_body() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"exists": {"body": true}}],
                "responses": [{"is": {"statusCode": 200, "body": "has body"}}]
            },
            {
                "predicates": [{"exists": {"body": false}}],
                "responses": [{"is": {"statusCode": 200, "body": "no body"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Request with body
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/test"))
        .body("some content")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "has body");

    // Request without body (GET typically has no body)
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "no body");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_exists_query_param() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"exists": {"query": {"token": true}}}],
            "responses": [{"is": {"statusCode": 200, "body": "has token"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test?token=abc123"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "has token");

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "has token");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_exists_header() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"exists": {"headers": {"authorization": true}}}],
            "responses": [{"is": {"statusCode": 200, "body": "authorized"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .header("Authorization", "Bearer token123")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "authorized");

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "authorized");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

/// exists predicate respects keyCaseSensitive (Issue #86 - fixed)
#[tokio::test]
#[ignore = "requires running server"]
async fn test_exists_key_case_sensitive() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "exists": {"query": {"Token": true}},
                "keyCaseSensitive": false
            }],
            "responses": [{"is": {"statusCode": 200, "body": "token found"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // keyCaseSensitive=false: predicate "Token" should match query "token"
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test?token=abc"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "exists predicate should respect keyCaseSensitive=false for query keys"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 6. Logical Combinators
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_not_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"not": {"equals": {"method": "GET"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "not GET"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // POST should match (not GET)
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "not GET");

    // GET should NOT match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "not GET");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_or_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "or": [
                    {"equals": {"path": "/api/v1"}},
                    {"equals": {"path": "/api/v2"}}
                ]
            }],
            "responses": [{"is": {"statusCode": 200, "body": "api version"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/v1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "api version");

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/v2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "api version");

    // v3 should not match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/v3"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "api version");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_and_predicate() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "and": [
                    {"equals": {"method": "POST"}},
                    {"startsWith": {"path": "/api/"}}
                ]
            }],
            "responses": [{"is": {"statusCode": 201, "body": "created"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Both conditions met
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/api/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Only one condition met
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 201);

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_nested_not_and_or() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // not(or(GET, DELETE)) => matches POST, PUT, PATCH, etc.
    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "not": {
                    "or": [
                        {"equals": {"method": "GET"}},
                        {"equals": {"method": "DELETE"}}
                    ]
                }
            }],
            "responses": [{"is": {"statusCode": 200, "body": "not get or delete"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // POST should match (not GET and not DELETE)
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "not get or delete");

    // GET should NOT match
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "not get or delete");

    // DELETE should NOT match
    let resp = client
        .delete(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "not get or delete");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 7. Response Types
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_is_response_with_headers() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {
                    "statusCode": 201,
                    "headers": {
                        "Content-Type": "application/json",
                        "X-Custom-Header": "custom-value"
                    },
                    "body": "{\"id\": 1}"
                }
            }]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
    assert_eq!(
        resp.headers()
            .get("x-custom-header")
            .unwrap()
            .to_str()
            .unwrap(),
        "custom-value"
    );
    assert_eq!(resp.text().await.unwrap(), "{\"id\": 1}");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_multiple_responses_round_robin() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [
                {"is": {"statusCode": 200, "body": "first"}},
                {"is": {"statusCode": 200, "body": "second"}},
                {"is": {"statusCode": 200, "body": "third"}}
            ]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let expected = ["first", "second", "third", "first", "second", "third"];
    for expected_body in &expected {
        let resp = client
            .get(format!("{ADMIN_URL}:{imposter_port}/test"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.text().await.unwrap(), *expected_body);
    }

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_fault_connection_reset() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{"fault": "CONNECTION_RESET_BY_PEER"}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // The connection should be reset, causing an error
    let result = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await;

    assert!(
        result.is_err(),
        "Fault response should cause connection error"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_default_response_empty_stubs() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "defaultResponse": {
            "statusCode": 418,
            "body": "I'm a teapot"
        },
        "stubs": []
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/anything"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 418);
    assert_eq!(resp.text().await.unwrap(), "I'm a teapot");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 8. Behaviors
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_behavior_wait() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {"statusCode": 200, "body": "waited"},
                "_behaviors": {"wait": 200}
            }]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let start = std::time::Instant::now();
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(resp.status(), 200);
    assert!(
        elapsed >= Duration::from_millis(180),
        "Wait behavior should delay at least 180ms, got {elapsed:?}"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_behavior_repeat() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [
                {
                    "is": {"statusCode": 200, "body": "repeated"},
                    "_behaviors": {"repeat": 3}
                },
                {"is": {"statusCode": 200, "body": "after repeat"}}
            ]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // First 3 should return "repeated"
    for _ in 0..3 {
        let resp = client
            .get(format!("{ADMIN_URL}:{imposter_port}/test"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.text().await.unwrap(), "repeated");
    }

    // 4th should return "after repeat"
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "after repeat");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 9. Admin API CRUD
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_create_and_get_imposter() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "name": "Test Imposter",
        "stubs": [{
            "predicates": [],
            "responses": [{"is": {"statusCode": 200, "body": "hello"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // GET /imposters/:port
    let resp = client
        .get(format!(
            "{ADMIN_URL}:{admin_port}/imposters/{imposter_port}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["port"], imposter_port);
    assert_eq!(body["protocol"], "http");
    assert_eq!(body["name"], "Test Imposter");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_list_imposters() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create two imposters
    let config1 = json!({
        "protocol": "http",
        "stubs": []
    });
    let config2 = json!({
        "protocol": "http",
        "stubs": []
    });

    let first = create_imposter(&client, admin_port, config1).await;
    let second = create_imposter(&client, admin_port, config2).await;
    assert_ne!(first, second);

    // GET /imposters
    let resp = client
        .get(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let imposters = body["imposters"]
        .as_array()
        .expect("Expected imposters array");
    assert!(imposters.len() >= 2);

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_delete_single_imposter() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": []
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // DELETE /imposters/:port
    let resp = client
        .delete(format!(
            "{ADMIN_URL}:{admin_port}/imposters/{imposter_port}"
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // Verify it's gone
    let resp = client
        .get(format!(
            "{ADMIN_URL}:{admin_port}/imposters/{imposter_port}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_delete_all_imposters() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": []
    });

    create_imposter(&client, admin_port, config).await;

    // DELETE /imposters
    let resp = client
        .delete(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // Verify all gone
    let resp = client
        .get(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let imposters = body["imposters"].as_array().unwrap();
    assert_eq!(imposters.len(), 0);

    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_put_overwrite_all_imposters() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create initial imposter
    let config = json!({
        "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "old"}}]}]
    });
    let imposter_port = create_imposter(&client, admin_port, config).await;

    // PUT /imposters to overwrite all
    let new_config = json!({
        "imposters": [{
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "new"}}]}]
        }]
    });

    let resp = client
        .put(format!("{ADMIN_URL}:{admin_port}/imposters"))
        .json(&new_config)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();

    // Only the new imposter is left. It is auto-assigned, so it may reuse the old one's port;
    // a single-entry list serving "new" proves the overwrite either way.
    let imposters = body["imposters"]
        .as_array()
        .expect("Expected imposters array");
    assert_eq!(imposters.len(), 1, "{body}");
    let new_port = imposters[0]["port"].as_u64().expect("port") as u16;
    if new_port != imposter_port {
        let resp = client
            .get(format!(
                "{ADMIN_URL}:{admin_port}/imposters/{imposter_port}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    // New imposter should exist
    let resp = client
        .get(format!("{ADMIN_URL}:{new_port}/anything"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "new");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_admin_recorded_requests() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "recordRequests": true,
        "stubs": [{
            "predicates": [],
            "responses": [{"is": {"statusCode": 200, "body": "ok"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Make some requests
    client
        .get(format!("{ADMIN_URL}:{imposter_port}/path1"))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{ADMIN_URL}:{imposter_port}/path2"))
        .body("test body")
        .send()
        .await
        .unwrap();

    // Retrieve recorded requests
    let resp = client
        .get(format!(
            "{ADMIN_URL}:{admin_port}/imposters/{imposter_port}"
        ))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let requests = body["requests"].as_array();
    assert!(
        requests.is_some(),
        "Recorded requests should be present when recordRequests is true"
    );
    let requests = requests.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["method"], "GET");
    assert_eq!(requests[0]["path"], "/path1");
    assert_eq!(requests[1]["method"], "POST");
    assert_eq!(requests[1]["path"], "/path2");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 10. Stub Ordering & Matching
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_first_matching_stub_wins() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"equals": {"path": "/test"}}],
                "responses": [{"is": {"statusCode": 200, "body": "first stub"}}]
            },
            {
                "predicates": [{"equals": {"path": "/test"}}],
                "responses": [{"is": {"statusCode": 200, "body": "second stub"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // First matching stub should win
    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "first stub");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_catch_all_stub() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"equals": {"path": "/specific"}}],
                "responses": [{"is": {"statusCode": 200, "body": "specific"}}]
            },
            {
                "predicates": [],
                "responses": [{"is": {"statusCode": 200, "body": "catch all"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/specific"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "specific");

    let resp = client
        .get(format!("{ADMIN_URL}:{imposter_port}/anything-else"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "catch all");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 11. Form Data
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_form_urlencoded_matching() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"form": {"username": "admin"}}}],
            "responses": [{"is": {"statusCode": 200, "body": "form matched"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "form matched");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_deep_equals_form_exact_fields() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [
            {
                "predicates": [{"deepEquals": {"form": {"username": "admin"}}}],
                "responses": [{"is": {"statusCode": 200, "body": "exact form"}}]
            },
            {
                "predicates": [],
                "responses": [{"is": {"statusCode": 404, "body": "no match"}}]
            }
        ]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    // Exact match (only username)
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "exact form");

    // Extra field - deepEquals should fail
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=admin&password=secret")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.text().await.unwrap(),
        "no match",
        "deepEquals form should fail with extra fields"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// 12. Predicate Selectors (jsonpath, xpath)
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_jsonpath_selector() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"body": "admin"},
                "jsonpath": {"selector": "$.user.role"}
            }],
            "responses": [{"is": {"statusCode": 200, "body": "admin user"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/check"))
        .header("content-type", "application/json")
        .body(r#"{"user": {"name": "John", "role": "admin"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "admin user");

    // Non-admin role
    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/check"))
        .header("content-type", "application/json")
        .body(r#"{"user": {"name": "Jane", "role": "viewer"}}"#)
        .send()
        .await
        .unwrap();
    assert_ne!(resp.text().await.unwrap(), "admin user");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_xpath_selector() {
    let (mut server, admin_port) = start_rift_server().await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    let config = json!({
        "protocol": "http",
        "stubs": [{
            "predicates": [{
                "equals": {"body": "John"},
                "xpath": {"selector": "//user/name"}
            }],
            "responses": [{"is": {"statusCode": 200, "body": "xml matched"}}]
        }]
    });

    let imposter_port = create_imposter(&client, admin_port, config).await;

    let resp = client
        .post(format!("{ADMIN_URL}:{imposter_port}/check"))
        .header("content-type", "application/xml")
        .body("<root><user><name>John</name></user></root>")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "xml matched");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}
