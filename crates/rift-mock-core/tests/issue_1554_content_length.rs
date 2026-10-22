//! Issue #1554: a `Content-Length` a response sets is corrected to the body's real byte length at
//! the one funnel every response is served through (`handle_imposter_request`), after behaviors,
//! as Mountebank's `postProcess` does (`baseHttpServer.js:63-65`).
//!
//! hyper trusts a user `content-length` beside a body of known length and writes it verbatim, so
//! before the fix a too-large value hung a keep-alive client, a too-small one truncated the body,
//! and a debug build panicked in hyper's connection task (`proto/h1/role.rs:734`) so the client got
//! no reply at all. These tests read the wire with a raw socket, because an HTTP client would hide
//! the framing; the HTTP/2 test uses hyper's own client to see what h2 does with the header.
//!
//! Ports come from the manager only (`test_port_uniqueness.rs`).

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use rift_mock_core::imposter::ImposterManager;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What one request put on the wire.
#[derive(Debug)]
struct Wire {
    status: u16,
    /// Every `content-length` value, in order.
    content_length: Vec<String>,
    transfer_encoding: Option<String>,
    headers: Vec<(String, String)>,
    /// The bytes after the head, as sent (still chunked when the response is).
    body: Vec<u8>,
    /// The server closed the connection before the read deadline. With `Connection: close` that
    /// is how every well-framed response ends; a hang does not.
    closed: bool,
}

impl Wire {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Send one `Connection: close` request and read until the server closes or 5 s pass.
async fn exchange(port: u16, method: &str, path: &str) -> Wire {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the imposter");
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write the request");
    let mut raw = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .is_ok_and(|read| read.is_ok());
    parse(&raw, closed)
}

fn parse(raw: &[u8], closed: bool) -> Wire {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Wire {
            status: 0,
            content_length: Vec::new(),
            transfer_encoding: None,
            headers: Vec::new(),
            body: raw.to_vec(),
            closed,
        };
    };
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.clone())
        .collect();
    let transfer_encoding = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"))
        .map(|(_, v)| v.clone());
    Wire {
        status,
        content_length,
        transfer_encoding,
        headers,
        body: raw[split + 4..].to_vec(),
        closed,
    }
}

/// Decode a chunked body. `None` when it is not well-formed chunked framing.
fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size + 2..)?;
    }
}

/// The response is framed by a single, correct `content-length` and carries `expected`.
fn framed_problem(wire: &Wire, expected: &[u8]) -> Option<String> {
    let want = expected.len().to_string();
    if wire.status == 0 {
        return Some(format!("no response at all: {wire:?}"));
    }
    if !wire.closed {
        return Some(format!("the response never ended (hang): {wire:?}"));
    }
    if wire.content_length != [want.clone()] {
        return Some(format!(
            "content-length {:?}, want [{want}] (the body has {} bytes)",
            wire.content_length,
            wire.body.len()
        ));
    }
    if wire.body != expected {
        return Some(format!(
            "body {:?}, want {:?}",
            String::from_utf8_lossy(&wire.body),
            String::from_utf8_lossy(expected)
        ));
    }
    None
}

/// The multiline PNG from mbTest `httpStubTest.js:317`; Mountebank serves its 274 bytes.
const MULTILINE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAGQAAAAyBAMAAABYG2ONAAAAFVBMVEUAAAD///9/f39fX1+fn58f\nHx8/Pz8rYiDqAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAAo0lEQVRIie2Qyw7CIBBFb2DwO5q+1o0L\n1w0NrrHRPQnV//8EAUl0ga1ujIs5CZAz4YYZAIZhmN/QQOkjzq3LLuv6xUrQHmTJGphcEGE9rUQ3\nY4bqqrDjAlgQoJK9Z8YBmFy8Gp8DeSeTfRSBCf2I6/JN5ORiRfrNiIfqh9S9SVPL27A1C0G4EX2e\nJR7J1iI7rbG0Vf4x0UwPW0Uh3i0bwzD/yR11mBj1DIKiVwAAAABJRU5ErkJggg==\n";

/// One stub per case, each on its own path.
fn cases() -> Vec<(&'static str, Value)> {
    vec![
        (
            "/text-too-large",
            json!({ "is": { "headers": { "Content-Length": 852 }, "body": "hello" } }),
        ),
        (
            "/text-too-small",
            json!({ "is": { "headers": { "Content-Length": 2 }, "body": "hello" } }),
        ),
        (
            "/lower-case-name",
            json!({ "is": { "headers": { "content-length": "852" }, "body": "hello" } }),
        ),
        (
            "/non-numeric",
            json!({ "is": { "headers": { "Content-Length": "abc" }, "body": "hello" } }),
        ),
        (
            "/two-values",
            json!({ "is": { "headers": { "Content-Length": ["2", "852"] }, "body": "hello" } }),
        ),
        (
            "/binary-too-large",
            json!({ "is": { "headers": { "Content-Length": 852 }, "body": "aGVsbG8=", "_mode": "binary" } }),
        ),
        (
            "/binary-too-small",
            json!({ "is": { "headers": { "Content-Length": 2 }, "body": "aGVsbG8=", "_mode": "binary" } }),
        ),
        (
            "/decorate-lengthens",
            json!({ "is": { "headers": { "Content-Length": 5 }, "body": "hello" },
                    "_behaviors": { "decorate": "config => { config.response.body = config.response.body + 'XYZ'; }" } }),
        ),
        (
            "/copy-shortens",
            json!({ "is": { "headers": { "Content-Length": 5 }, "body": "${P}" },
                    "_behaviors": { "copy": { "from": "path", "into": "${P}",
                                              "using": { "method": "regex", "selector": "^/c" } } } }),
        ),
        (
            "/shell-transform",
            json!({ "is": { "headers": { "Content-Length": 5 }, "body": "hello" },
                    // The printed response replaces the `is` (#1589), so it carries the stale length.
                    "_behaviors": { "shellTransform":
                        r#"printf '{"headers":{"Content-Length":5},"body":"shell-output"}' #"# } }),
        ),
        (
            "/inject",
            json!({ "inject": "function (config) { return { statusCode: 200, headers: { 'Content-Length': 852 }, body: 'hello' }; }" }),
        ),
        (
            "/fault-error",
            json!({ "is": { "body": "never-seen" },
                    "_rift": { "fault": { "error": { "probability": 1.0, "status": 503, "body": "down",
                                                     "headers": { "Content-Length": "852" } } } } }),
        ),
    ]
}

/// What each case must put on the wire as its body.
fn expected_body(path: &str) -> Vec<u8> {
    match path {
        "/decorate-lengthens" => b"helloXYZ".to_vec(),
        "/copy-shortens" => b"/c".to_vec(),
        "/shell-transform" => b"shell-output".to_vec(),
        "/fault-error" => b"down".to_vec(),
        _ => b"hello".to_vec(),
    }
}

async fn imposter(manager: &ImposterManager, extra: Vec<Value>) -> u16 {
    let mut stubs: Vec<Value> = cases()
        .into_iter()
        .map(|(path, response)| {
            json!({ "predicates": [{ "equals": { "path": path } }], "responses": [response] })
        })
        .collect();
    stubs.extend(extra);
    let config = serde_json::from_value(json!({ "protocol": "http", "stubs": stubs }))
        .expect("imposter config");
    manager.create_imposter(config).await.expect("create")
}

#[tokio::test]
async fn every_response_source_is_framed_by_the_bodys_real_length() {
    let manager = ImposterManager::new();
    let port = imposter(&manager, Vec::new()).await;
    let mut problems = Vec::new();
    for (path, _) in cases() {
        let wire = exchange(port, "GET", path).await;
        if let Some(problem) = framed_problem(&wire, &expected_body(path)) {
            problems.push(format!("{path}: {problem}"));
        }
    }
    let _ = manager.delete_imposter(port).await;
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

// A JSON object body is serialized by the engine, so its length is whatever that serialization is:
// the header must count those bytes, and they must be the object.
#[tokio::test]
async fn a_json_body_is_framed_by_its_serialized_length() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(json!({ "protocol": "http", "stubs": [{ "responses": [
        { "is": { "headers": { "Content-Length": 2 }, "body": { "a": 1 } } }
    ] }] }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let wire = exchange(port, "GET", "/").await;
    let _ = manager.delete_imposter(port).await;
    assert!(wire.closed, "the response must end: {wire:?}");
    assert_eq!(
        wire.content_length,
        [wire.body.len().to_string()],
        "{wire:?}"
    );
    let body: Value = serde_json::from_slice(&wire.body).expect("the whole JSON body arrives");
    assert_eq!(body, json!({ "a": 1 }));
}

#[tokio::test]
async fn a_default_response_is_framed_by_the_bodys_real_length() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(json!({ "protocol": "http", "stubs": [],
        "defaultResponse": { "headers": { "Content-Length": 852 }, "body": "hello" } }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let wire = exchange(port, "GET", "/anything").await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(framed_problem(&wire, b"hello"), None);
}

// Mountebank strips the line breaks before decoding, so this decodes to 274 bytes rather than
// being served as its raw text with `x-rift-binary-error`.
#[tokio::test]
async fn a_multiline_base64_body_decodes_and_is_framed_by_its_decoded_length() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(json!({ "protocol": "http", "stubs": [{ "responses": [
        { "is": { "headers": { "Content-Length": 274 }, "_mode": "binary", "body": MULTILINE_PNG_BASE64 } }
    ] }] }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let wire = exchange(port, "GET", "/").await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(wire.header("x-rift-binary-error"), None, "{wire:?}");
    assert!(wire.closed, "the response must end: {wire:?}");
    assert_eq!(wire.content_length, ["274"], "{wire:?}");
    assert_eq!(wire.body.len(), 274);
    assert_eq!(&wire.body[..4], b"\x89PNG");
}

// HTTP forbids `Content-Length` beside `Transfer-Encoding`, and hyper cancels such a response, so the
// client used to get an empty reply. The length is dropped and the body is sent chunked.
#[tokio::test]
async fn content_length_beside_transfer_encoding_is_dropped() {
    let manager = ImposterManager::new();
    let config = serde_json::from_value(json!({ "protocol": "http", "stubs": [{ "responses": [
        { "is": { "headers": { "Content-Length": 852, "Transfer-Encoding": "chunked" }, "body": "hello" } }
    ] }] }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    let wire = exchange(port, "GET", "/").await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(wire.status, 200, "a response must arrive: {wire:?}");
    assert!(wire.content_length.is_empty(), "{wire:?}");
    assert_eq!(wire.transfer_encoding.as_deref(), Some("chunked"));
    assert_eq!(dechunk(&wire.body).as_deref(), Some(&b"hello"[..]));
}

// A HEAD response carries the length a GET would get (RFC 9110 §9.3.2) and no body.
#[tokio::test]
async fn a_head_response_carries_the_get_length_and_no_body() {
    let manager = ImposterManager::new();
    let port = imposter(&manager, Vec::new()).await;
    let wire = exchange(port, "HEAD", "/text-too-large").await;
    let _ = manager.delete_imposter(port).await;
    assert!(wire.closed, "{wire:?}");
    assert_eq!(wire.content_length, ["5"], "{wire:?}");
    assert!(wire.body.is_empty(), "{wire:?}");
}

// No body on the wire for 204 and 304 whatever the stub says. A 204 must not carry a length at all
// (RFC 9110 §8.6), as hyper itself sends it when no header is set; a 304 carries the length the body
// would have had. An empty body is framed as 0.
#[tokio::test]
async fn bodiless_statuses_and_an_empty_body() {
    let manager = ImposterManager::new();
    let extra = vec![
        json!({ "predicates": [{ "equals": { "path": "/204" } }], "responses": [
            { "is": { "statusCode": 204, "headers": { "Content-Length": 852 }, "body": "hello" } } ] }),
        json!({ "predicates": [{ "equals": { "path": "/304" } }], "responses": [
            { "is": { "statusCode": 304, "headers": { "Content-Length": 852 }, "body": "hello" } } ] }),
        json!({ "predicates": [{ "equals": { "path": "/empty" } }], "responses": [
            { "is": { "headers": { "Content-Length": 852 }, "body": "" } } ] }),
    ];
    let port = imposter(&manager, extra).await;
    let no_content = exchange(port, "GET", "/204").await;
    let not_modified = exchange(port, "GET", "/304").await;
    let empty = exchange(port, "GET", "/empty").await;
    let _ = manager.delete_imposter(port).await;

    assert_eq!(no_content.status, 204, "{no_content:?}");
    assert!(no_content.closed, "{no_content:?}");
    assert!(no_content.content_length.is_empty(), "{no_content:?}");
    assert!(no_content.body.is_empty(), "{no_content:?}");

    assert_eq!(not_modified.status, 304, "{not_modified:?}");
    assert!(not_modified.closed, "{not_modified:?}");
    assert_eq!(not_modified.content_length, ["5"], "{not_modified:?}");
    assert!(not_modified.body.is_empty(), "{not_modified:?}");

    assert_eq!(framed_problem(&empty, b""), None);
}

// A keep-alive client is where a stale length did its damage: too large and the client waits for
// bytes that never come; too small and the rest of the body is read as the next response. Two
// requests on one connection must both come back whole.
#[tokio::test]
async fn a_keep_alive_connection_survives_two_responses() {
    let manager = ImposterManager::new();
    let port = imposter(&manager, Vec::new()).await;
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = |path: &str, close: bool| {
        format!(
            "GET {path} HTTP/1.1\r\nHost: x\r\n{}\r\n",
            if close { "Connection: close\r\n" } else { "" }
        )
    };
    stream
        .write_all(request("/text-too-small", false).as_bytes())
        .await
        .expect("first request");
    stream
        .write_all(request("/text-too-large", true).as_bytes())
        .await
        .expect("second request");
    let mut raw = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .is_ok_and(|read| read.is_ok());
    let _ = manager.delete_imposter(port).await;
    let text = String::from_utf8_lossy(&raw);
    assert!(closed, "the second response must end: {text:?}");
    assert_eq!(
        text.matches("HTTP/1.1 200 OK").count(),
        2,
        "two whole responses: {text:?}"
    );
    assert!(text.ends_with("\r\n\r\nhello"), "{text:?}");
    assert_eq!(text.matches("Content-Length: 5\r\n").count(), 2, "{text:?}");
}

/// HTTP/2 through hyper's own client, which enforces `content-length` against the DATA it reads.
async fn h2_get(port: u16, path: &str) -> Result<(Option<String>, Bytes), String> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (mut sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(conn);
    let request = hyper::Request::get(format!("http://127.0.0.1:{port}{path}"))
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("request: {e}"))?;
    let response = tokio::time::timeout(Duration::from_secs(5), sender.send_request(request))
        .await
        .map_err(|_| "response head timed out".to_owned())?
        .map_err(|e| format!("send: {e}"))?;
    let length = response
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let body = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .map_err(|_| "body timed out".to_owned())?
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    Ok((length, body))
}

// HTTP/2 has no `Transfer-Encoding` (hyper strips it) and frames the body with DATA frames, but it
// still sends the `content-length` header, and the client checks it against the DATA: a stale value
// failed the stream. Corrected, every case reads whole; beside `Transfer-Encoding` the header hyper
// sets from the body is what arrives.
#[tokio::test]
async fn http2_sends_the_corrected_length() {
    let manager = ImposterManager::new();
    let extra = vec![
        json!({ "predicates": [{ "equals": { "path": "/te" } }], "responses": [
        { "is": { "headers": { "Content-Length": 852, "Transfer-Encoding": "chunked" }, "body": "hello" } } ] }),
    ];
    let port = imposter(&manager, extra).await;
    let mut problems = Vec::new();
    for path in [
        "/text-too-large",
        "/text-too-small",
        "/binary-too-large",
        "/decorate-lengthens",
        "/te",
    ] {
        let expected = expected_body(path);
        match h2_get(port, path).await {
            Ok((length, body)) => {
                if length != Some(expected.len().to_string()) || body != expected {
                    problems.push(format!("{path}: content-length {length:?}, body {body:?}"));
                }
            }
            Err(e) => problems.push(format!("{path}: {e}")),
        }
    }
    let _ = manager.delete_imposter(port).await;
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// hyper's debug-build check on a user `content-length` (`proto/h1/role.rs:734`). Before #1554 a
/// stale stub value tripped it, panicking the connection task, so the client got no reply.
#[cfg(debug_assertions)]
#[tokio::test]
async fn a_stale_length_no_longer_trips_hypers_debug_assertion() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static HYPER_LENGTH_PANICS: AtomicUsize = AtomicUsize::new(0);
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info
                .to_string()
                .contains("custom content-length header claims")
            {
                HYPER_LENGTH_PANICS.fetch_add(1, Ordering::SeqCst);
            }
            previous(info);
        }));
    });

    let manager = ImposterManager::new();
    let port = imposter(&manager, Vec::new()).await;
    let mut no_reply = Vec::new();
    for path in ["/text-too-large", "/text-too-small", "/binary-too-large"] {
        let wire = exchange(port, "GET", path).await;
        if wire.status != 200 {
            no_reply.push(path);
        }
    }
    let head = exchange(port, "HEAD", "/text-too-large").await;
    let _ = manager.delete_imposter(port).await;
    assert_eq!(
        HYPER_LENGTH_PANICS.load(Ordering::SeqCst),
        0,
        "hyper's debug assertion fired on a stale content-length"
    );
    assert!(no_reply.is_empty(), "no reply for {no_reply:?}");
    assert_eq!(head.status, 200, "HEAD: {head:?}");
}
