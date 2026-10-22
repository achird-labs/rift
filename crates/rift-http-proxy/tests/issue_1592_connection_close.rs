//! Issue #1592: Rift keeps imposter connections alive where Mountebank closes them.
//!
//! Mountebank adds `Connection: close` to every response that does not set one. Rift does not
//! (docs/mountebank/differences.md#connections-stay-open). These tests pin both halves of that
//! entry over raw sockets: the default keeps the connection open, and the documented opt-in, a
//! `Connection: close` in a stub's or the `defaultResponse`'s headers, makes Rift send the header
//! and close the socket.
//!
//! Every imposter takes an engine-assigned port.

use std::time::Duration;

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// Send one `GET` on `stream` and read until the two-byte body `ok` has arrived.
async fn get(stream: &mut TcpStream, path: &str) -> String {
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut text = String::new();
    let mut buf = [0u8; 1024];
    while !text.contains("\r\n\r\nok") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("response arrives")
            .expect("read");
        assert!(
            n > 0,
            "connection closed before the response completed: {text:?}"
        );
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    text
}

/// After a complete response: `true` when the server closed the socket (EOF), `false` when it is
/// still open (nothing arrives within the window).
async fn closed_by_server(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 16];
    match timeout(Duration::from_millis(1500), stream.read(&mut buf)).await {
        Ok(Ok(0)) => true,
        Ok(Ok(_)) => panic!("unexpected bytes after the response"),
        // A reset is the peer closing as well.
        Ok(Err(_)) => true,
        Err(_) => false,
    }
}

fn has_connection_close(response: &str) -> bool {
    response
        .lines()
        .any(|l| l.to_ascii_lowercase().replace(' ', "") == "connection:close")
}

fn ok_stub(path: &str, headers: Value) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "is": { "statusCode": 200, "headers": headers, "body": "ok" } }]
    })
}

#[tokio::test]
async fn a_response_without_connection_keeps_the_socket_open() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ok_stub("/a", json!({})), ok_stub("/b", json!({}))] }),
    )
    .await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let first = get(&mut stream, "/a").await;
    assert!(
        !has_connection_close(&first),
        "no Connection header by default: {first:?}"
    );
    assert!(
        !closed_by_server(&mut stream).await,
        "the connection must stay open"
    );
    let second = get(&mut stream, "/b").await;
    assert!(
        second.starts_with("HTTP/1.1 200"),
        "second request on the same socket: {second:?}"
    );
}

#[tokio::test]
async fn a_stub_connection_close_closes_the_socket() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "protocol": "http", "stubs": [ok_stub("/a", json!({ "Connection": "close" }))] }),
    )
    .await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let first = get(&mut stream, "/a").await;
    assert!(
        has_connection_close(&first),
        "Connection: close is sent: {first:?}"
    );
    assert!(
        closed_by_server(&mut stream).await,
        "Rift closes the socket after the response"
    );
}

#[tokio::test]
async fn a_default_response_connection_close_closes_the_socket() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "protocol": "http",
            "defaultResponse": {
                "statusCode": 200,
                "headers": { "Connection": "close" },
                "body": "ok"
            },
            "stubs": []
        }),
    )
    .await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let first = get(&mut stream, "/nothing-matches").await;
    assert!(
        has_connection_close(&first),
        "Connection: close is sent: {first:?}"
    );
    assert!(
        closed_by_server(&mut stream).await,
        "Rift closes the socket after the response"
    );
}
