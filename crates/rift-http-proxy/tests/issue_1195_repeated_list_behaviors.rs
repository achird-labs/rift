//! Issue #1195: Mountebank spells several `copy` behaviors as one `behaviors` element each and runs
//! them all, in order. Rift folded the array key by key with the last element winning, so a
//! Mountebank file with two `copy` elements served only the second.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::json;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn start_admin() -> (reqwest::Client, String) {
    let admin_port = free_port();
    let server = AdminApiServer::new(
        format!("127.0.0.1:{admin_port}").parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    );
    tokio::spawn(server.run());
    let admin = format!("http://127.0.0.1:{admin_port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{admin}/imposters"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (client, admin)
}

/// The Mountebank 2.9.1 probe from the triage, verbatim: it serves `A=cop B=GET` for `GET /copy`.
#[tokio::test]
async fn two_copy_elements_both_run_as_in_mountebank() {
    let (client, admin) = start_admin().await;
    let port = free_port();
    let created = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "port": port,
            "protocol": "http",
            "stubs": [{ "responses": [{
                "is": { "body": "A=${A} B=${B}" },
                "behaviors": [
                    { "copy": { "from": "path", "into": "${A}",
                                "using": { "method": "regex", "selector": "c.p" } } },
                    { "copy": { "from": "method", "into": "${B}",
                                "using": { "method": "regex", "selector": ".+" } } }
                ]
            }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    assert_eq!(created.status().as_u16(), 201);

    let body = client
        .get(format!("http://127.0.0.1:{port}/copy"))
        .send()
        .await
        .expect("GET /copy")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "A=cop B=GET");
}

/// A malformed item in an earlier element is live now, so the admin door refuses it instead of
/// admitting a file whose first copy silently did nothing.
#[tokio::test]
async fn a_malformed_earlier_copy_element_is_refused_at_the_door() {
    let (client, admin) = start_admin().await;
    let response = client
        .post(format!("{admin}/imposters"))
        .json(&json!({
            "protocol": "http",
            "stubs": [{ "responses": [{
                "is": { "body": "x" },
                "behaviors": [
                    { "copy": { "into": "${A}", "using": { "method": "regex", "selector": ".+" } } },
                    { "copy": { "from": "path", "into": "${B}",
                                "using": { "method": "regex", "selector": ".+" } } }
                ]
            }] }]
        }))
        .send()
        .await
        .expect("POST /imposters");
    let status = response.status().as_u16();
    let body = response.text().await.expect("body");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("`copy` behavior is malformed"), "{body}");
}
