//! Issue #1493: a request no admin route takes gets Express's `finalhandler` 404 page, as Mountebank
//! 2.9.5 answers it, while a Mountebank handler's own 404 (an absent imposter) stays JSON.
//!
//! Every expectation was measured against `mb` 2.9.5 first.

use std::sync::Arc;

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::ImposterManager;

const NOPE_PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot GET /nope</pre>\n</body>\n</html>\n";

async fn start() -> (String, RunningAdminApi) {
    let running = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::new(ImposterManager::new()),
        None,
    )
    .bind()
    .await
    .expect("admin binds");
    (format!("http://{}", running.local_addr()), running)
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii header").to_string())
}

fn page(shown: &str) -> String {
    NOPE_PAGE.replace("Cannot GET /nope", shown)
}

async fn send(base: &str, method: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
            format!("{base}{path}"),
        )
        .send()
        .await
        .expect("send")
}

#[tokio::test]
async fn an_unknown_path_is_finalhandlers_page() {
    let (base, admin) = start().await;
    let response = send(&base, "GET", "/nope").await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        header(&response, "content-security-policy").as_deref(),
        Some("default-src 'none'")
    );
    assert_eq!(
        header(&response, "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(header(&response, "content-length").as_deref(), Some("143"));
    assert_eq!(header(&response, "vary"), None);
    assert_eq!(response.text().await.expect("body"), NOPE_PAGE);
    admin.shutdown().await;
}

#[tokio::test]
async fn head_on_an_unknown_path_is_the_headers_of_cannot_head() {
    let (base, admin) = start().await;
    let response = send(&base, "HEAD", "/nope").await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&response, "content-length").as_deref(), Some("144"));
    assert_eq!(response.text().await.expect("body"), "");
    admin.shutdown().await;
}

#[tokio::test]
async fn a_known_path_with_a_method_it_lacks_is_the_page() {
    let (base, admin) = start().await;
    for (method, path) in [("POST", "/config"), ("PATCH", "/imposters")] {
        let response = send(&base, method, path).await;
        assert_eq!(response.status(), 404, "{method} {path}");
        assert_eq!(
            response.text().await.expect("body"),
            page(&format!("Cannot {method} {path}")),
            "{method} {path}"
        );
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn the_message_is_the_raw_path_without_its_query_encoded_and_escaped() {
    let (base, admin) = start().await;
    for (sent, shown) in [
        ("/nope?x=1", "/nope"),
        ("/NoPe/", "/NoPe/"),
        ("/a'%22&b", "/a&#39;%22&amp;b"),
        ("/imposters//", "/imposters//"),
    ] {
        let response = send(&base, "GET", sent).await;
        assert_eq!(response.status(), 404, "{sent}");
        assert_eq!(
            response.text().await.expect("body"),
            page(&format!("Cannot GET {shown}")),
            "{sent}"
        );
    }
    admin.shutdown().await;
}

#[tokio::test]
async fn a_handler_404_for_an_absent_imposter_stays_json() {
    let (base, admin) = start().await;
    for path in ["/imposters/9999", "/imposters/abc"] {
        let response = send(&base, "GET", path).await;
        assert_eq!(response.status(), 404, "{path}");
        let body: serde_json::Value = response.json().await.expect("json");
        assert_eq!(body["errors"][0]["type"], "no such resource", "{path}");
    }
    admin.shutdown().await;
}
