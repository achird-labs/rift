//! Issue #1610: a recorded request's `timestamp` is `Date.toJSON()`'s shape in Mountebank
//! (`2026-10-17T10:00:00.123Z`: milliseconds, literal `Z`). Rift printed `to_rfc3339`'s
//! `…123456+00:00`, with a fraction of varying length.

use std::sync::Arc;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

#[tokio::test]
async fn a_recorded_request_is_stamped_in_millisecond_z_form() {
    let manager = Arc::new(ImposterManager::new());
    let server = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None);
    let running = server.bind().await.expect("admin API binds");
    let admin = format!("http://{}", running.local_addr());
    let client = reqwest::Client::new();

    let created: Value = client
        .post(format!("{admin}/imposters"))
        .json(&json!({"protocol": "http", "recordRequests": true,
            "stubs": [{"responses": [{"is": {"statusCode": 200}}]}]}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let port = created["port"].as_u64().expect("assigned port");

    client
        .get(format!("http://127.0.0.1:{port}/x"))
        .send()
        .await
        .expect("request");

    let imposter: Value = client
        .get(format!("{admin}/imposters/{port}"))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let stamp = imposter["requests"][0]["timestamp"]
        .as_str()
        .expect("timestamp is a string");

    let shape = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$").expect("regex");
    assert!(shape.is_match(stamp), "timestamp was {stamp:?}");

    let parsed = chrono::DateTime::parse_from_rfc3339(stamp).expect("RFC 3339");
    let age = chrono::Utc::now().signed_duration_since(parsed);
    assert!(
        age.num_seconds().abs() < 10,
        "timestamp {stamp:?} is not close to now"
    );
}
