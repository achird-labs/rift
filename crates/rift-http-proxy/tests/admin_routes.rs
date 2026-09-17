//! `ADMIN_ROUTES` against a live admin listener: every entry is dispatched, and the intercept
//! family is served only when the server was built `with_intercept`.
//!
//! "Dispatched" means the router did not answer its own `404`. A handler may still answer `400`
//! (no body sent) or a `404` of its own ("flow-state key not found") — those are routes that exist.
//! The router's fall-through `404` is the only one whose message is exactly `Not Found`, which is
//! how the two are told apart.

use std::sync::Arc;
use std::time::Duration;

use rift_http_proxy::admin_api::{ADMIN_ROUTES, AdminApiServer, AdminRoute, RouteFamily};
use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::intercept_control::InterceptControl;

const PORT: u16 = 23471;

fn imposter() -> serde_json::Value {
    serde_json::json!({
        "port": PORT, "protocol": "http",
        "_rift": { "flowState": { "backend": "inmemory", "ttlSeconds": 300 } },
        "stubs": [
            { "id": "a", "scenarioName": "order", "requiredScenarioState": "Started",
              "responses": [{ "is": { "statusCode": 200 } }] }
        ]
    })
}

fn concrete(entry: &AdminRoute) -> String {
    concrete_path(entry.path)
}

fn concrete_path(path: &str) -> String {
    path.replace("{port}", &PORT.to_string())
        .replace("{stubIndex}", "0")
        .replace("{stubId}", "a")
        .replace("{scenario}", "order")
        .replace("{space}", "f1")
        .replace("{key}", "k")
}

async fn send(client: &reqwest::Client, base: &str, entry: &AdminRoute) -> reqwest::Response {
    let method = reqwest::Method::from_bytes(entry.method.as_str().as_bytes()).expect("method");
    client
        .request(method, format!("{base}{}", concrete(entry)))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{} {}: {e}", entry.method, entry.path))
}

/// Whether a response is the router's own fall-through `404`, i.e. no route matched.
async fn is_router_404(response: reqwest::Response) -> bool {
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        return false;
    }
    let body: serde_json::Value = response.json().await.expect("error body is json");
    body["errors"][0]["message"] == "Not Found"
}

#[tokio::test]
async fn every_table_entry_is_dispatched() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .with_intercept(InterceptControl::default())
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let client = reqwest::Client::new();

    for entry in ADMIN_ROUTES {
        // Several entries delete the imposter or every imposter; each request sees it present.
        manager
            .apply_config(vec![serde_json::from_value(imposter()).expect("config")])
            .await
            .expect("restore the sample imposter");

        if entry.family == RouteFamily::Events {
            // A stream never ends; its head is the proof it was dispatched.
            let response =
                tokio::time::timeout(Duration::from_secs(5), send(&client, &base, entry))
                    .await
                    .unwrap_or_else(|_| panic!("{} {} never answered", entry.method, entry.path));
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            assert!(
                content_type.starts_with("text/event-stream"),
                "{} {} answered {} with {content_type:?}",
                entry.method,
                entry.path,
                response.status()
            );
            continue;
        }

        let response = send(&client, &base, entry).await;
        assert!(
            !is_router_404(response).await,
            "{} {} fell through to the router's 404",
            entry.method,
            entry.path
        );
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn intercept_entries_are_404_without_with_intercept() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let client = reqwest::Client::new();

    let intercept: Vec<_> = ADMIN_ROUTES
        .iter()
        .filter(|r| r.family == RouteFamily::Intercept)
        .collect();
    assert!(!intercept.is_empty(), "the family is populated");
    for entry in intercept {
        let response = send(&client, &base, entry).await;
        assert!(
            is_router_404(response).await,
            "{} {} is served without with_intercept",
            entry.method,
            entry.path
        );
    }
    admin.shutdown().await;
}

/// The other direction: every method the table does *not* list on a listed path is the router's
/// 404. This is what catches a method added to an existing route without a table entry — and it is
/// what showed the event streams answering every method, before they were made `GET`-only.
#[tokio::test]
async fn no_unlisted_method_is_dispatched_on_a_listed_path() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .with_intercept(InterceptControl::default())
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let client = reqwest::Client::new();
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

    let mut paths: Vec<&str> = ADMIN_ROUTES.iter().map(|r| r.path).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        for method in methods {
            if ADMIN_ROUTES
                .iter()
                .any(|r| r.path == path && r.method.as_str() == method)
            {
                continue;
            }
            manager
                .apply_config(vec![serde_json::from_value(imposter()).expect("config")])
                .await
                .expect("restore the sample imposter");
            let url = format!("{base}{}", concrete_path(path));
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                client
                    .request(reqwest::Method::from_bytes(method.as_bytes()).expect("method"), url)
                    .send(),
            )
            .await
            .unwrap_or_else(|_| panic!("{method} {path} never answered"))
            .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
            let status = response.status();
            // `HEAD` has no body to carry the message, so its status alone is the evidence.
            let routed_away = if method == "HEAD" {
                status == reqwest::StatusCode::NOT_FOUND
            } else {
                is_router_404(response).await
            };
            assert!(routed_away, "{method} {path} is dispatched ({status}) but not listed");
        }
    }
    admin.shutdown().await;
}

/// `intercept_entries_are_404_without_with_intercept` would pass against a router that 404s
/// everything; this is the control that says the discriminator can see a real miss.
#[tokio::test]
async fn an_unlisted_path_is_the_router_404() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let response = reqwest::Client::new()
        .get(format!("{base}/no-such-route"))
        .send()
        .await
        .expect("send");
    assert!(is_router_404(response).await);
    admin.shutdown().await;
}
