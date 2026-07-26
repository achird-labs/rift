//! Integration tests for Rift extensions (`_rift` namespace features)
//!
//! These tests verify that the `_rift` configuration extensions work correctly
//! for flow state, scripting, and fault injection.

mod support;

use reqwest::Client;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const ADMIN_URL: &str = "http://127.0.0.1";
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Helper to get a free port for testing
fn get_test_ports() -> (u16, u16) {
    // Use high ports to avoid conflicts
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT_COUNTER: AtomicU16 = AtomicU16::new(18000);
    let admin = PORT_COUNTER.fetch_add(2, Ordering::SeqCst);
    let imposter = admin + 1;
    (admin, imposter)
}

/// Start a Rift server for testing
async fn start_rift_server(admin_port: u16) -> tokio::process::Child {
    let child = tokio::process::Command::new(support::server_bin())
        .args(["--port", &admin_port.to_string(), "--allow-injection"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("Failed to start Rift server");

    // Wait for server to be ready
    let client = Client::new();
    for _ in 0..50 {
        if client
            .get(format!("{ADMIN_URL}:{admin_port}/"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .is_ok()
        {
            return child;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("Rift server failed to start within timeout");
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
// Flow State Integration Tests
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_flow_state_inmemory_basic() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with flow state and inject script that uses state
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "_rift": {
            "flowState": {
                "backend": "inmemory",
                "ttlSeconds": 300
            }
        },
        "stubs": [{
            "predicates": [],
            "responses": [{
                "inject": "function(request, state) { state.counter = (state.counter || 0) + 1; return { statusCode: 200, body: 'Count: ' + state.counter }; }"
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    // Make multiple requests and verify state is maintained
    for i in 1..=3 {
        let response = client
            .get(format!("{ADMIN_URL}:{imposter_port}/test"))
            .send()
            .await
            .expect("Request failed");

        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert_eq!(body, format!("Count: {i}"));
    }

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_flow_state_persistence_across_requests() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with flow state that stores user data
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "_rift": {
            "flowState": {
                "backend": "inmemory"
            }
        },
        "stubs": [
            {
                "predicates": [{"equals": {"method": "POST", "path": "/store"}}],
                "responses": [{
                    "inject": "function(request, state) { var data = JSON.parse(request.body); state.stored = data.value; return { statusCode: 201, body: 'Stored' }; }"
                }]
            },
            {
                "predicates": [{"equals": {"method": "GET", "path": "/retrieve"}}],
                "responses": [{
                    "inject": "function(request, state) { return { statusCode: 200, body: state.stored || 'empty' }; }"
                }]
            }
        ]
    });

    create_imposter(&client, admin_port, config).await;

    // First retrieve - should be empty
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/retrieve"))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.text().await.unwrap(), "empty");

    // Store a value
    let response = client
        .post(format!("{ADMIN_URL}:{imposter_port}/store"))
        .body(r#"{"value": "test-data"}"#)
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.status(), 201);

    // Retrieve again - should have stored value
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/retrieve"))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.text().await.unwrap(), "test-data");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// Fault Injection Integration Tests
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_fault_latency_100_percent() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with 100% probability latency fault
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "body": "delayed response"
                },
                "_rift": {
                    "fault": {
                        "latency": {
                            "probability": 1.0,
                            "ms": 200
                        }
                    }
                }
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    let start = Instant::now();
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .expect("Request failed");
    let elapsed = start.elapsed();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "delayed response");
    assert!(
        elapsed >= Duration::from_millis(180),
        "Expected at least 180ms delay, got {elapsed:?}"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_fault_latency_range() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with latency range
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "body": "ok"
                },
                "_rift": {
                    "fault": {
                        "latency": {
                            "probability": 1.0,
                            "minMs": 100,
                            "maxMs": 200
                        }
                    }
                }
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    let start = Instant::now();
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .expect("Request failed");
    let elapsed = start.elapsed();

    assert_eq!(response.status(), 200);
    assert!(
        elapsed >= Duration::from_millis(90),
        "Expected at least 90ms delay, got {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_millis(300),
        "Expected at most 300ms delay, got {elapsed:?}"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_fault_error_100_percent() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with 100% error fault
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "body": "normal response"
                },
                "_rift": {
                    "fault": {
                        "error": {
                            "probability": 1.0,
                            "status": 503,
                            "body": "Service Unavailable"
                        }
                    }
                }
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(response.status(), 503);
    assert_eq!(response.text().await.unwrap(), "Service Unavailable");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_rift_fault_probabilistic() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with 50% error probability
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "body": "success"
                },
                "_rift": {
                    "fault": {
                        "error": {
                            "probability": 0.5,
                            "status": 500,
                            "body": "error"
                        }
                    }
                }
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    let mut success_count = 0;
    let mut error_count = 0;

    // Make 100 requests to get a statistical sample
    for _ in 0..100 {
        let response = client
            .get(format!("{ADMIN_URL}:{imposter_port}/test"))
            .send()
            .await
            .expect("Request failed");

        if response.status() == 200 {
            success_count += 1;
        } else if response.status() == 500 {
            error_count += 1;
        }
    }

    // With 50% probability, we should see roughly equal distribution
    // Allow for some statistical variance (expect between 25% and 75% of either)
    assert!(
        (25..=75).contains(&success_count),
        "Expected roughly 50% success rate, got {success_count} successes and {error_count} errors"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

// =============================================================================
// Mountebank Compatibility Tests with _rift Extensions
// =============================================================================

#[tokio::test]
#[ignore = "requires running server"]
async fn test_mountebank_behaviors_with_rift_fault() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with both Mountebank _behaviors and _rift fault
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [{"equals": {"path": "/test"}}],
            "responses": [{
                "is": {
                    "statusCode": 200,
                    "headers": {"X-Custom": "header"},
                    "body": "response with both behaviors"
                },
                "_behaviors": {
                    "wait": 50
                },
                "_rift": {
                    "fault": {
                        "latency": {
                            "probability": 1.0,
                            "ms": 50
                        }
                    }
                }
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    let start = Instant::now();
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/test"))
        .send()
        .await
        .expect("Request failed");
    let elapsed = start.elapsed();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("X-Custom")
            .map(|v| v.to_str().unwrap()),
        Some("header")
    );
    // Both waits should apply: 50ms from _behaviors + 50ms from _rift
    assert!(
        elapsed >= Duration::from_millis(80),
        "Expected at least 80ms combined delay, got {elapsed:?}"
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_mountebank_predicates_with_rift_extensions() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with complex Mountebank predicates and _rift extensions
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "_rift": {
            "flowState": {"backend": "inmemory"}
        },
        "stubs": [
            {
                "predicates": [
                    {"equals": {"method": "GET"}},
                    {"startsWith": {"path": "/api/"}}
                ],
                "responses": [{
                    "is": {
                        "statusCode": 200,
                        "body": "API response"
                    },
                    "_rift": {
                        "fault": {
                            "latency": {"probability": 1.0, "ms": 10}
                        }
                    }
                }]
            },
            {
                "predicates": [{"equals": {"method": "POST"}}],
                "responses": [{
                    "is": {
                        "statusCode": 201,
                        "body": "Created"
                    }
                }]
            }
        ]
    });

    create_imposter(&client, admin_port, config).await;

    // Test GET /api/* - should match first stub with _rift latency
    let start = Instant::now();
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/api/users"))
        .send()
        .await
        .expect("Request failed");
    let elapsed = start.elapsed();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "API response");
    assert!(elapsed >= Duration::from_millis(5)); // Should have some delay

    // Test POST - should match second stub without _rift
    let response = client
        .post(format!("{ADMIN_URL}:{imposter_port}/create"))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.status(), 201);

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_response_cycling_with_rift_extensions() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with multiple responses that cycle, each with different _rift configs
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "stubs": [{
            "predicates": [],
            "responses": [
                {
                    "is": {"statusCode": 200, "body": "first"},
                    "_rift": {"fault": {"latency": {"probability": 1.0, "ms": 10}}}
                },
                {
                    "is": {"statusCode": 200, "body": "second"}
                },
                {
                    "is": {"statusCode": 200, "body": "third"},
                    "_rift": {"fault": {"latency": {"probability": 1.0, "ms": 10}}}
                }
            ]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    // Sequential dispatch: concurrent requests reach the round-robin counter in
    // nondeterministic order, which makes the ordering assertion below unsound.
    let mut bodies: Vec<String> = Vec::with_capacity(6);
    for _ in 0..6 {
        let body = client
            .get(format!("{ADMIN_URL}:{imposter_port}/test"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        bodies.push(body);
    }

    assert_eq!(
        bodies,
        vec!["first", "second", "third", "first", "second", "third"]
    );

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}

#[tokio::test]
#[ignore = "requires running server"]
async fn test_default_response_with_rift_config() {
    let (admin_port, imposter_port) = get_test_ports();
    let mut server = start_rift_server(admin_port).await;
    let client = Client::builder().timeout(TEST_TIMEOUT).build().unwrap();

    // Create imposter with default response and _rift flow state
    let config = json!({
        "port": imposter_port,
        "protocol": "http",
        "_rift": {
            "flowState": {"backend": "inmemory"}
        },
        "defaultResponse": {
            "statusCode": 404,
            "body": "Not found"
        },
        "stubs": [{
            "predicates": [{"equals": {"path": "/exists"}}],
            "responses": [{
                "is": {"statusCode": 200, "body": "Found"}
            }]
        }]
    });

    create_imposter(&client, admin_port, config).await;

    // Request to existing path
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/exists"))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "Found");

    // Request to non-existing path - should use default response
    let response = client
        .get(format!("{ADMIN_URL}:{imposter_port}/nonexistent"))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "Not found");

    clear_imposters(&client, admin_port).await;
    server.kill().await.ok();
}
