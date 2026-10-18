//! Issue #1554 through the shared listeners: the admin API's `/__rift/` gateway and the front door
//! (a matched route and its `/__rift/` fallback) serve an imposter through the same funnel as its
//! own port, so a stale stub `Content-Length` is corrected there too. Read with a raw socket, since
//! an HTTP client would hide the framing.

use arc_swap::ArcSwap;
use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::front_door::{
    CompiledRoutes, Route, RouteMatch, RouteTable, RouteTarget, bind_front_door,
};
use rift_http_proxy::imposter::ImposterManager;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `(status line, content-length values, body bytes, closed before the deadline)`.
async fn exchange(
    addr: SocketAddr,
    method: &str,
    path: &str,
) -> (String, Vec<String>, Vec<u8>, bool) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .is_ok_and(|read| read.is_ok());
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (String::new(), Vec::new(), raw, closed);
    };
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default().to_owned();
    let lengths = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().to_owned())
        .collect();
    (status, lengths, raw[split + 4..].to_vec(), closed)
}

#[tokio::test]
async fn the_gateway_and_the_front_door_correct_a_stale_content_length() {
    let manager = Arc::new(ImposterManager::new());
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "headers": { "Content-Length": 852 }, "body": "hello" } }] }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");

    let admin = AdminApiServer::new("127.0.0.1:0".parse().unwrap(), manager.clone(), None)
        .bind()
        .await
        .expect("bind admin API");
    let table = RouteTable {
        routes: vec![Route {
            id: "routed".to_owned(),
            priority: 0,
            matches: RouteMatch {
                path_prefix: Some("/routed".to_owned()),
                ..RouteMatch::default()
            },
            target: RouteTarget {
                port,
                strip_prefix: true,
                set_host: None,
            },
            enabled: true,
        }],
    };
    let compiled = Arc::new(ArcSwap::new(Arc::new(CompiledRoutes::new(&table))));
    let front = bind_front_door("127.0.0.1:0".parse().unwrap(), manager.clone(), compiled)
        .await
        .expect("bind front door");

    let doors = [
        (
            "admin /__rift/",
            admin.local_addr(),
            format!("/__rift/{port}/x"),
        ),
        (
            "front door /__rift/",
            front.local_addr(),
            format!("/__rift/{port}/x"),
        ),
        (
            "front door route",
            front.local_addr(),
            "/routed/x".to_owned(),
        ),
    ];
    let mut problems = Vec::new();
    for (door, addr, path) in &doors {
        let (status, lengths, body, closed) = exchange(*addr, "GET", path).await;
        if !status.contains(" 200 ") || lengths != ["5"] || body != b"hello" || !closed {
            problems.push(format!(
                "{door} GET: {status:?} content-length {lengths:?} body {:?} closed {closed}",
                String::from_utf8_lossy(&body)
            ));
        }
        let (status, lengths, body, closed) = exchange(*addr, "HEAD", path).await;
        if !status.contains(" 200 ") || lengths != ["5"] || !body.is_empty() || !closed {
            problems.push(format!(
                "{door} HEAD: {status:?} content-length {lengths:?} body {} bytes closed {closed}",
                body.len()
            ));
        }
    }
    let _ = manager.delete_imposter(port).await;
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
