//! Issue #1550: what a proxy sends upstream for each form of `to`. Mountebank's `getProxyRequest`
//! takes only the host and port from `to` and sends the request's own path and query, so a trailing
//! `/`, a `?query` or a `#fragment` in `to` changes nothing. Rift used to append the request to `to`
//! as a string: `to` + `/` sent `//foo`, `?q=1` folded the request into the query, and `#f` dropped
//! the path.
//!
//! The upstream is a raw socket that records the request line as sent, because an HTTP server that
//! parses `//foo` as an authority would hide the bug. `defaultForward` is a Rift extension documented
//! as a base URL, so it keeps its path, joined without a doubled `/`.
//!
//! Ports come from `:0` binds only (`test_port_uniqueness.rs`).

use rift_mock_core::imposter::{ImposterConfig, ImposterManager, deserialize_replayed};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A raw HTTP/1.1 upstream that keeps the head of every request it receives and answers `200 up`.
struct RawUpstream {
    port: u16,
    heads: Arc<Mutex<Vec<String>>>,
}

impl RawUpstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&heads);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let kept = Arc::clone(&kept);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    kept.lock()
                        .expect("heads lock")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nup",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { port, heads }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The single request head received since the last call.
    fn take_one(&self) -> String {
        let mut heads = self.heads.lock().expect("heads lock");
        assert_eq!(heads.len(), 1, "exactly one upstream request: {heads:?}");
        heads.remove(0)
    }
}

fn request_line(head: &str) -> &str {
    head.lines().next().unwrap_or_default()
}

async fn send(port: u16, path_and_query: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path_and_query}"))
        .send()
        .await
        .expect("send to the imposter")
}

/// An imposter with one `proxyTransparent` stub, decoded as a replay would (so a `to` the config
/// doors now refuse can still be served, as it is for a config stored by an earlier engine).
fn replayed_proxy(proxy: Value) -> ImposterConfig {
    deserialize_replayed(json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "proxy": proxy }] }]
    }))
    .expect("replayed decode")
}

async fn forwarded_by_stub(proxy: Value, client_path: &str) -> String {
    let upstream = RawUpstream::start().await;
    let mut proxy = proxy;
    let to = proxy["to"]
        .as_str()
        .expect("to")
        .replace("UPSTREAM", &upstream.base());
    proxy["to"] = Value::String(to);
    proxy["mode"] = json!("proxyTransparent");
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(replayed_proxy(proxy))
        .await
        .expect("create");
    let response = send(port, client_path).await;
    assert_eq!(response.status(), 200, "proxied");
    assert_eq!(response.text().await.expect("body"), "up");
    let head = upstream.take_one();
    let _ = manager.delete_imposter(port).await;
    head
}

async fn forwarded_by_default_forward(base: &str, client_path: &str) -> String {
    let upstream = RawUpstream::start().await;
    let config: ImposterConfig = serde_json::from_value(json!({
        "protocol": "http",
        "defaultForward": base.replace("UPSTREAM", &upstream.base()),
        "stubs": []
    }))
    .expect("defaultForward config");
    let manager = ImposterManager::new();
    let port = manager.create_imposter(config).await.expect("create");
    let response = send(port, client_path).await;
    assert_eq!(response.status(), 200, "forwarded");
    let head = upstream.take_one();
    let _ = manager.delete_imposter(port).await;
    head
}

/// The triage's forwarding table: each `to` form, and what Mountebank sends for `GET /foo/bar?a=1`.
/// The path-bearing forms are refused at a config door now; served from a stored config, they
/// forward what Mountebank's `getProxyRequest` would.
#[tokio::test]
async fn a_stub_proxy_forwards_the_requests_own_path_and_query_whatever_follows_the_host() {
    for to in [
        "UPSTREAM",
        "UPSTREAM/",
        "UPSTREAM?q=1",
        "UPSTREAM/#f",
        "UPSTREAM/base",
        "UPSTREAM/base/",
    ] {
        let head = forwarded_by_stub(json!({ "to": to }), "/foo/bar?a=1").await;
        assert_eq!(
            request_line(&head),
            "GET /foo/bar?a=1 HTTP/1.1",
            "to = {to}"
        );
    }
}

/// Userinfo in `to` is not forwarded either: Mountebank reads `auth` off a WHATWG `URL`, which has
/// none, so no `Authorization` is sent.
#[tokio::test]
async fn a_stub_proxy_does_not_forward_userinfo_from_to() {
    let upstream = RawUpstream::start().await;
    let to = upstream.base().replace("http://", "http://user:pw@");
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(replayed_proxy(
            json!({ "to": to, "mode": "proxyTransparent" }),
        ))
        .await
        .expect("create");
    assert_eq!(send(port, "/foo").await.status(), 200);
    let head = upstream.take_one();
    assert_eq!(request_line(&head), "GET /foo HTTP/1.1");
    assert!(
        !head.to_ascii_lowercase().contains("\r\nauthorization:"),
        "no Authorization from the userinfo: {head}"
    );
    let _ = manager.delete_imposter(port).await;
}

/// `pathRewrite` with `"from": ""` is the supported way to add a base path, and is applied to the
/// request path before the join.
#[tokio::test]
async fn path_rewrite_from_empty_prefixes_the_upstream_path() {
    let head = forwarded_by_stub(
        json!({ "to": "UPSTREAM/", "pathRewrite": { "from": "", "to": "/base" } }),
        "/foo/bar?a=1",
    )
    .await;
    assert_eq!(request_line(&head), "GET /base/foo/bar?a=1 HTTP/1.1");
}

/// `defaultForward` is a base URL: its path is kept, a trailing `/` on it is not doubled, and a
/// query or fragment on it is ignored.
#[tokio::test]
async fn default_forward_joins_its_base_path_without_a_doubled_slash() {
    for (base, expected) in [
        ("UPSTREAM", "GET /foo?a=1 HTTP/1.1"),
        ("UPSTREAM/", "GET /foo?a=1 HTTP/1.1"),
        ("UPSTREAM/api", "GET /api/foo?a=1 HTTP/1.1"),
        ("UPSTREAM/api/", "GET /api/foo?a=1 HTTP/1.1"),
        ("UPSTREAM/api?x=1", "GET /api/foo?a=1 HTTP/1.1"),
        ("UPSTREAM/#f", "GET /foo?a=1 HTTP/1.1"),
    ] {
        let head = forwarded_by_default_forward(base, "/foo?a=1").await;
        assert_eq!(request_line(&head), expected, "defaultForward = {base}");
    }
}

/// A `to` that does not parse can only come from a stored or programmatic config; it answers the
/// proxy-error 502, never a panic or a silent pass.
#[tokio::test]
async fn an_unparseable_to_answers_the_proxy_error() {
    let manager = ImposterManager::new();
    let port = manager
        .create_imposter(replayed_proxy(json!({ "to": "not a url" })))
        .await
        .expect("create");
    let response = send(port, "/foo").await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        response
            .headers()
            .get("x-rift-proxy-error")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    let _ = manager.delete_imposter(port).await;
}
