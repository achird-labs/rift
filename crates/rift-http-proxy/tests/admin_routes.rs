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

/// An imposter port nothing binds: the intercept routes are refused before any port lookup.
const UNUSED_PORT: u16 = 1;

fn imposter() -> serde_json::Value {
    serde_json::json!({
        "protocol": "http",
        "_rift": { "flowState": { "ttlSeconds": 300 } },
        "stubs": [
            { "id": "a", "scenarioName": "order", "requiredScenarioState": "Started",
              "responses": [{ "is": { "statusCode": 200 } }] }
        ]
    })
}

/// Replace whatever the previous request left with one fresh sample imposter, on a port the
/// manager assigns.
async fn restore(manager: &ImposterManager) -> u16 {
    manager.delete_all().await;
    manager
        .create_imposter(serde_json::from_value(imposter()).expect("config"))
        .await
        .expect("restore the sample imposter")
}

fn concrete(entry: &AdminRoute, port: u16) -> String {
    concrete_path(entry.path, port)
}

fn concrete_path(path: &str, port: u16) -> String {
    path.replace("{port}", &port.to_string())
        .replace("{stubIndex}", "0")
        .replace("{stubId}", "a")
        .replace("{scenario}", "order")
        .replace("{space}", "f1")
        .replace("{key}", "k")
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    port: u16,
    entry: &AdminRoute,
) -> reqwest::Response {
    let method = reqwest::Method::from_bytes(entry.method.as_str().as_bytes()).expect("method");
    client
        .request(method, format!("{base}{}", concrete(entry, port)))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{} {}: {e}", entry.method, entry.path))
}

/// Whether a response is the router's own fall-through `404`, i.e. no route matched: Express's
/// `finalhandler` page, not a handler's JSON envelope.
async fn is_router_404(response: reqwest::Response) -> bool {
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        return false;
    }
    let is_html = response
        .headers()
        .get("content-type")
        .is_some_and(|v| v == "text/html; charset=utf-8");
    let body = response.text().await.expect("body");
    is_html && body.contains("<pre>Cannot ")
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
        let port = restore(&manager).await;

        if entry.family == RouteFamily::Events {
            // A stream never ends; its head is the proof it was dispatched.
            let response =
                tokio::time::timeout(Duration::from_secs(5), send(&client, &base, port, entry))
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

        let response = send(&client, &base, port, entry).await;
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
    let port = UNUSED_PORT;

    let intercept: Vec<_> = ADMIN_ROUTES
        .iter()
        .filter(|r| r.family == RouteFamily::Intercept)
        .collect();
    assert!(!intercept.is_empty(), "the family is populated");
    for entry in intercept {
        let response = send(&client, &base, port, entry).await;
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
///
/// `HEAD` and `OPTIONS` are never listed: every path answers them (issue #1456), and
/// `head_and_options_follow_the_table` below checks those answers instead.
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
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];

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
            let port = restore(&manager).await;
            let url = format!("{base}{}", concrete_path(path, port));
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                client
                    .request(
                        reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
                        url,
                    )
                    .send(),
            )
            .await
            .unwrap_or_else(|_| panic!("{method} {path} never answered"))
            .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
            let status = response.status();
            assert!(
                is_router_404(response).await,
                "{method} {path} is dispatched ({status}) but not listed"
            );
        }
    }
    admin.shutdown().await;
}

/// Every listed path answers `OPTIONS` with exactly its listed methods plus `HEAD` where `GET` is
/// listed, and `HEAD` with the `GET`'s status, or the 404 Express gives a path with no `GET`.
/// A stream's `GET` never ends, but its head arrives, so its status is still there to compare.
#[tokio::test]
async fn head_and_options_follow_the_table() {
    let manager = Arc::new(ImposterManager::new());
    let admin = AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
        .with_intercept(InterceptControl::default())
        .bind()
        .await
        .expect("admin binds");
    let base = format!("http://{}", admin.local_addr());
    let client = reqwest::Client::new();
    let port = restore(&manager).await;

    let mut paths: Vec<&str> = ADMIN_ROUTES.iter().map(|r| r.path).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        let url = format!("{base}{}", concrete_path(path, port));
        let mut listed: Vec<&str> = ADMIN_ROUTES
            .iter()
            .filter(|r| r.path == path)
            .map(|r| r.method.as_str())
            .collect();
        let has_get = listed.contains(&"GET");
        if has_get {
            listed.push("HEAD");
        }
        listed.sort_unstable();

        let options = client
            .request(reqwest::Method::OPTIONS, &url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("OPTIONS {path}: {e}"));
        assert_eq!(options.status(), 200, "OPTIONS {path}");
        let allow = options
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(allow, listed.join(", "), "OPTIONS {path}");

        let head = tokio::time::timeout(Duration::from_secs(5), client.head(&url).send())
            .await
            .unwrap_or_else(|_| panic!("HEAD {path} never answered"))
            .unwrap_or_else(|e| panic!("HEAD {path}: {e}"));
        if has_get {
            let get = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
                .await
                .unwrap_or_else(|_| panic!("GET {path} never answered"))
                .unwrap_or_else(|e| panic!("GET {path}: {e}"));
            assert_eq!(head.status(), get.status(), "HEAD {path}");
        } else {
            assert_eq!(head.status(), 404, "HEAD {path}");
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
