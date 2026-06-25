//! Rift-only step definitions for advanced scenario tests
//!
//! These steps test Rift-specific features that are not available in Mountebank.

use crate::world::{CompatibilityWorld, Service};
use cucumber::{gherkin::Step, given, then, when};
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Saved values for cross-step assertions
    static SAVED_VALUES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// Previous headers for comparison assertions
    static PREVIOUS_HEADERS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

// ==========================================================================
// Given Steps
// ==========================================================================

#[given(expr = "Rift service is running")]
async fn rift_service_running(world: &mut CompatibilityWorld) {
    world.ensure_containers().await.expect("Failed to start containers");

    // Verify Rift is accessible
    let rift_check = world.client
        .get(format!("{}/", world.config.rift_admin_url))
        .send()
        .await;

    assert!(rift_check.is_ok(), "Rift is not accessible");
}

#[given(expr = "all imposters are cleared on Rift")]
async fn clear_rift_imposters(world: &mut CompatibilityWorld) {
    world.client
        .delete(format!("{}/imposters", world.config.rift_admin_url))
        .send()
        .await
        .expect("Failed to clear Rift imposters");
    world.clear_response_sequence();

    // Clear saved values
    SAVED_VALUES.with(|sv| sv.borrow_mut().clear());
    PREVIOUS_HEADERS.with(|ph| ph.borrow_mut().clear());
}

#[given(expr = "an imposter on port {int} on Rift with:")]
async fn create_rift_imposter(world: &mut CompatibilityWorld, port: u16, step: &Step) {
    let config = step.docstring().expect("Missing docstring").to_string();

    // Don't adjust port - both services create at the same port numbers
    // Docker port mapping handles the offset for access (host 5545 -> container 4545)
    let json: serde_json::Value = serde_json::from_str(&config)
        .expect("Invalid JSON in imposter config");

    world.client
        .post(format!("{}/imposters", world.config.rift_admin_url))
        .header("Content-Type", "application/json")
        .body(json.to_string())
        .send()
        .await
        .expect("Failed to create Rift imposter");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

// ==========================================================================
// When Steps
// ==========================================================================

#[when(expr = "I send GET request to {string} on Rift imposter {int}")]
async fn send_get_to_rift(world: &mut CompatibilityWorld, path: String, port: u16) {
    let url = format!("{}{}", world.get_imposter_url(port, Service::Rift), path);

    let start = std::time::Instant::now();
    let response = world.client
        .get(&url)
        .send()
        .await
        .expect("Failed to send GET request to Rift");
    let duration = start.elapsed();

    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response.text().await.unwrap_or_default();

    // Store response for Then steps
    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: headers.clone(),
        rift_duration: duration,
    });

    // Save headers for "same header" assertions
    PREVIOUS_HEADERS.with(|ph| *ph.borrow_mut() = headers);
}

#[when(expr = "I send GET request to {string} on Rift imposter {int} and measure time")]
async fn send_get_to_rift_measure(world: &mut CompatibilityWorld, path: String, port: u16) {
    send_get_to_rift(world, path, port).await;
}

#[when(expr = "I send GET request to {string} with header {string} on Rift imposter {int}")]
async fn send_get_with_header_to_rift(world: &mut CompatibilityWorld, path: String, header: String, port: u16) {
    let url = format!("{}{}", world.get_imposter_url(port, Service::Rift), path);

    let parts: Vec<&str> = header.splitn(2, ": ").collect();
    let mut request = world.client.get(&url);

    if parts.len() == 2 {
        request = request.header(parts[0], parts[1]);
    }

    let start = std::time::Instant::now();
    let response = request.send().await.expect("Failed to send GET request");
    let duration = start.elapsed();

    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: headers.clone(),
        rift_duration: duration,
    });

    PREVIOUS_HEADERS.with(|ph| *ph.borrow_mut() = headers);
}

#[when(expr = "I send POST request to {string} on Rift imposter {int}")]
async fn send_post_to_rift(world: &mut CompatibilityWorld, path: String, port: u16) {
    let url = format!("{}{}", world.get_imposter_url(port, Service::Rift), path);

    let start = std::time::Instant::now();
    let response = world.client
        .post(&url)
        .send()
        .await
        .expect("Failed to send POST request to Rift");
    let duration = start.elapsed();

    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: headers.clone(),
        rift_duration: duration,
    });

    PREVIOUS_HEADERS.with(|ph| *ph.borrow_mut() = headers);
}

#[when(expr = "I send POST request with body {string} to {string} on Rift imposter {int}")]
async fn send_post_with_body_to_rift(world: &mut CompatibilityWorld, body: String, path: String, port: u16) {
    let url = format!("{}{}", world.get_imposter_url(port, Service::Rift), path);

    let start = std::time::Instant::now();
    let response = world.client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .expect("Failed to send POST request to Rift");
    let duration = start.elapsed();

    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let resp_body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: resp_body,
        rift_headers: headers.clone(),
        rift_duration: duration,
    });

    PREVIOUS_HEADERS.with(|ph| *ph.borrow_mut() = headers);
}

#[when(expr = "I send POST request with header {string} to {string} on Rift imposter {int}")]
async fn send_post_with_header_to_rift(world: &mut CompatibilityWorld, header: String, path: String, port: u16) {
    let url = format!("{}{}", world.get_imposter_url(port, Service::Rift), path);

    let parts: Vec<&str> = header.splitn(2, ": ").collect();
    let mut request = world.client.post(&url);

    if parts.len() == 2 {
        request = request.header(parts[0], parts[1]);
    }

    let start = std::time::Instant::now();
    let response = request.send().await.expect("Failed to send POST request");
    let duration = start.elapsed();

    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: headers.clone(),
        rift_duration: duration,
    });

    PREVIOUS_HEADERS.with(|ph| *ph.borrow_mut() = headers);
}

// ==========================================================================
// Then Steps
// ==========================================================================

// Note: "Rift should return status" step is defined in then.rs

#[then(expr = "Rift response body should be {string}")]
async fn check_rift_body(world: &mut CompatibilityWorld, expected: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    assert_eq!(response.rift_body.trim(), expected, "Rift body mismatch: expected '{}', got '{}'", expected, response.rift_body.trim());
}

#[then(expr = "Rift response body should contain {string}")]
async fn check_rift_body_contains(world: &mut CompatibilityWorld, substring: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    assert!(response.rift_body.contains(&substring), "Rift body '{}' does not contain '{}'", response.rift_body, substring);
}

#[then(expr = "Rift response should have header {string} with value {string}")]
async fn check_rift_header(world: &mut CompatibilityWorld, header: String, value: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    let header_lower = header.to_lowercase();
    let actual = response.rift_headers.get(&header_lower);
    assert_eq!(actual.map(|s| s.as_str()), Some(value.as_str()),
        "Rift header '{}' mismatch: expected '{}', got {:?}", header, value, actual);
}

#[then(expr = "Rift response should have header {string}")]
async fn check_rift_has_header(world: &mut CompatibilityWorld, header: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    let header_lower = header.to_lowercase();
    assert!(response.rift_headers.contains_key(&header_lower),
        "Rift response missing header '{}'. Available: {:?}", header, response.rift_headers.keys().collect::<Vec<_>>());
}

#[then(expr = "Rift response should take at least {int}ms")]
async fn check_rift_response_time_min(world: &mut CompatibilityWorld, min_ms: u64) {
    let response = world.last_response.as_ref().expect("No response recorded");
    let min_duration = std::time::Duration::from_millis(min_ms);
    assert!(response.rift_duration >= min_duration,
        "Rift response too fast: {:?} < {:?}", response.rift_duration, min_duration);
}

#[then(expr = "Rift response should take at most {int}ms")]
async fn check_rift_response_time_max(world: &mut CompatibilityWorld, max_ms: u64) {
    let response = world.last_response.as_ref().expect("No response recorded");
    let max_duration = std::time::Duration::from_millis(max_ms);
    assert!(response.rift_duration <= max_duration,
        "Rift response too slow: {:?} > {:?}", response.rift_duration, max_duration);
}

#[then(expr = "I save the Rift response body as {string}")]
async fn save_rift_body(world: &mut CompatibilityWorld, key: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    SAVED_VALUES.with(|sv| sv.borrow_mut().insert(key, response.rift_body.clone()));
}

#[then(expr = "Rift response body should match saved {string}")]
async fn check_rift_body_matches_saved(world: &mut CompatibilityWorld, key: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    SAVED_VALUES.with(|sv| {
        let saved = sv.borrow().get(&key).cloned().expect("No saved value found");
        assert_eq!(response.rift_body.trim(), saved.trim(),
            "Rift body doesn't match saved value '{}': expected '{}', got '{}'", key, saved, response.rift_body);
    });
}

#[then(expr = "Rift response should have same header {string} as previous request")]
async fn check_rift_header_same_as_previous(world: &mut CompatibilityWorld, header: String) {
    let response = world.last_response.as_ref().expect("No response recorded");
    let header_lower = header.to_lowercase();

    PREVIOUS_HEADERS.with(|ph| {
        let previous = ph.borrow().get(&header_lower).cloned()
            .expect("No previous header value found");
        let current = response.rift_headers.get(&header_lower)
            .expect("Current response missing header");

        assert_eq!(current, &previous,
            "Rift header '{}' changed: was '{}', now '{}'", header, previous, current);
    });
}

// ==========================================================================
// Admin API Steps
// ==========================================================================

#[when(expr = "I query {string} on Rift admin API")]
async fn query_rift_admin(world: &mut CompatibilityWorld, path: String) {
    let url = format!("{}{}", world.config.rift_admin_url, path);

    let response = world.client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request to Rift admin API");

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: HashMap::new(),
        rift_duration: std::time::Duration::ZERO,
    });
}

// ==========================================================================
// Script Validation Steps
// ==========================================================================

#[when(expr = "I try to create an imposter on Rift with:")]
async fn try_create_rift_imposter(world: &mut CompatibilityWorld, step: &Step) {
    let config = step.docstring().expect("Missing docstring").to_string();

    let json: serde_json::Value = serde_json::from_str(&config)
        .expect("Invalid JSON in imposter config");

    let response = world.client
        .post(format!("{}/imposters", world.config.rift_admin_url))
        .header("Content-Type", "application/json")
        .body(json.to_string())
        .send()
        .await
        .expect("Failed to send request to Rift");

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: HashMap::new(),
        rift_duration: std::time::Duration::ZERO,
    });
}

#[when(expr = "I try to add a stub to imposter {int} on Rift with:")]
async fn try_add_stub_to_rift(world: &mut CompatibilityWorld, port: u16, step: &Step) {
    let config = step.docstring().expect("Missing docstring").to_string();

    let json: serde_json::Value = serde_json::from_str(&config)
        .expect("Invalid JSON in stub config");

    let response = world.client
        .post(format!("{}/imposters/{}/stubs", world.config.rift_admin_url, port))
        .header("Content-Type", "application/json")
        .body(json.to_string())
        .send()
        .await
        .expect("Failed to send request to Rift");

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    world.last_response = Some(crate::world::DualResponse {
        mb_status: 0,
        mb_body: String::new(),
        mb_headers: HashMap::new(),
        mb_duration: std::time::Duration::ZERO,
        rift_status: status,
        rift_body: body,
        rift_headers: HashMap::new(),
        rift_duration: std::time::Duration::ZERO,
    });
}
