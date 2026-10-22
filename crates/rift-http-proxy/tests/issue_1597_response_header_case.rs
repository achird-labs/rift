//! Issue #1597: HTTP/1 response header names go out Title-Case.
//!
//! Mountebank writes a stub's header names exactly as spelled; hyper has no public API to do
//! that, so Rift's documented approximation is Title-Case (docs/mountebank/differences.md,
//! "Response header names"). A reqwest client folds names to lower case, so every assertion here
//! reads the response head off a raw socket. HTTP/2 names are lower-case by protocol.
//!
//! Every imposter and the front door take engine-assigned ports.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rift_http_proxy::front_door::{CompiledRoutes, RouteTable, bind_front_door};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

fn config(with_tcp_fault: bool) -> Value {
    let mut stubs = vec![json!({
        "predicates": [{ "equals": { "path": "/h" } }],
        "responses": [{ "is": {
            "statusCode": 200,
            "headers": { "x-custom": "a", "Content-TYPE": "text/plain", "lower-hdr": "b" },
            "body": "ok"
        } }]
    })];
    if with_tcp_fault {
        // A TCP fault anywhere in the imposter forces the http1-only builder.
        stubs.push(json!({
            "predicates": [{ "equals": { "path": "/fault" } }],
            "responses": [{ "fault": "CONNECTION_RESET_BY_PEER" }]
        }));
    }
    json!({ "protocol": "http", "stubs": stubs })
}

async fn create(manager: &ImposterManager, with_tcp_fault: bool) -> u16 {
    let cfg = serde_json::from_value(config(with_tcp_fault)).expect("config");
    let port = manager.create_imposter(cfg).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// GET `target` over a raw HTTP/1.1 socket and return the response head (status line + headers).
async fn raw_head(addr: std::net::SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .expect("response arrives")
        .expect("read");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    text.split("\r\n\r\n").next().unwrap_or_default().to_owned()
}

fn assert_title_case(head: &str) {
    for expected in [
        "X-Custom: a",
        "Content-Type: text/plain",
        "Lower-Hdr: b",
        "X-Rift-Imposter: true",
        "Content-Length: 2",
    ] {
        assert!(head.contains(expected), "missing {expected:?} in {head:?}");
    }
    for line in head.lines().skip(1) {
        assert!(
            line.starts_with(|c: char| c.is_ascii_uppercase()),
            "header line must start upper-case: {line:?} in {head:?}"
        );
    }
}

#[tokio::test]
async fn the_auto_listener_title_cases_response_header_names() {
    let manager = ImposterManager::new();
    let port = create(&manager, false).await;
    let head = raw_head(([127, 0, 0, 1], port).into(), "/h").await;
    assert_title_case(&head);
}

#[tokio::test]
async fn the_http1_only_listener_title_cases_response_header_names() {
    let manager = ImposterManager::new();
    let port = create(&manager, true).await;
    let head = raw_head(([127, 0, 0, 1], port).into(), "/h").await;
    assert_title_case(&head);
}

#[tokio::test]
async fn the_front_door_title_cases_response_header_names() {
    for with_tcp_fault in [false, true] {
        let manager = Arc::new(ImposterManager::new());
        let port = create(&manager, with_tcp_fault).await;
        let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(
            &RouteTable::default(),
        ))));
        let running = bind_front_door("127.0.0.1:0".parse().unwrap(), manager, compiled)
            .await
            .expect("bind front door");
        let head = raw_head(running.local_addr(), &format!("/__rift/{port}/h")).await;
        assert_title_case(&head);
        running.shutdown().await;
    }
}

#[tokio::test]
async fn http2_names_stay_lower_case() {
    use http_body_util::Empty;
    use hyper::body::Bytes;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let manager = ImposterManager::new();
    let port = create(&manager, false).await;
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .expect("h2c handshake");
    tokio::spawn(conn);
    let request = hyper::Request::get(format!("http://127.0.0.1:{port}/h"))
        .body(Empty::<Bytes>::new())
        .expect("request");
    let response = sender.send_request(request).await.expect("h2 response");
    assert_eq!(response.status(), 200);
    // `HeaderMap` stores lower-case by type; the point is that the h2 leg still serves (names
    // are lower-case on the wire by RFC 9113 §8.2.2, enforced by h2 itself).
    assert_eq!(response.headers().get("x-custom").expect("x-custom"), "a");
}
