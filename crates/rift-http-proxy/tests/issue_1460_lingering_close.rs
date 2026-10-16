//! Issue #1460: a request body over the limit is answered `413` with `connection: close`, and that
//! answer must reach the client.
//!
//! Before the fix every door refused with the body still arriving, then closed the socket with
//! unread data in it. The kernel turns that close into a RST, and a client still writing its body
//! sees "connection reset" instead of the `413`. The fix is a lingering close: once the refusal is
//! written the listener shuts its write side (the client sees the `413`, then a FIN) and discards
//! what the client is still sending, for a bounded time, before closing.
//!
//! Raw sockets on purpose: an HTTP client library decides for itself whether a body-write error
//! outranks a response it already received, and that choice is exactly what hid the bug. Every
//! probe here writes the way such a client does — part of the body, a pause long enough for the
//! refusal to be written, then the rest — and asserts both halves: the write completes and the
//! read yields the `413`.

use arc_swap::ArcSwap;
use clap::Parser;
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::front_door::{
    CompiledRoutes, Route, RouteMatch, RouteTable, RouteTarget, RunningFrontDoor, bind_front_door,
};
use rift_http_proxy::imposter::ImposterManager;
use rift_http_proxy::imposter::body_limits::BodyLimits;
use rift_http_proxy::server::{Cli, ServerBuilder};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

const LIMIT: usize = 1024 * 1024;
/// Four times the limit: far more than hyper's one drain read can swallow, so on the unfixed
/// listener most of it is still unread when the socket closes, yet small enough to finish well
/// inside the 2 s drain on a loaded machine.
const DECLARED: usize = 4 * 1024 * 1024;
/// Written before the pause; enough to be well into the body, under the limit.
const FIRST: usize = 256 * 1024;
/// Long enough that the refusal is written and the connection closed before the rest is sent.
const PAUSE: Duration = Duration::from_millis(300);
const READ_BUDGET: Duration = Duration::from_secs(10);

fn limited_manager() -> Arc<ImposterManager> {
    Arc::new(ImposterManager::new().with_body_limits(BodyLimits::new(LIMIT, None).expect("limits")))
}

async fn imposter(manager: &ImposterManager, protocol: &str) -> u16 {
    let config = serde_json::from_value(serde_json::json!({
        "protocol": protocol,
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
    }))
    .expect("config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn start_admin(manager: Arc<ImposterManager>) -> RunningAdminApi {
    AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager, None)
        .bind()
        .await
        .expect("bind admin API")
}

async fn start_front_door(manager: Arc<ImposterManager>, port: u16) -> RunningFrontDoor {
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
    bind_front_door("127.0.0.1:0".parse().expect("addr"), manager, compiled)
        .await
        .expect("bind front door")
}

fn content_length_head(path: &str) -> String {
    format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {DECLARED}\r\n\r\n")
}

/// Send `head` and a `DECLARED`-byte body the way a real client does, then read to EOF.
///
/// `Err` names which half failed, so a red run says whether the write was reset or the read was.
async fn oversize_probe<S>(mut stream: S, head: &str) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("writing the head: {e}"))?;
    let first = vec![b'x'; FIRST];
    stream
        .write_all(&first)
        .await
        .map_err(|e| format!("writing the first {FIRST} bytes: {e}"))?;
    tokio::time::sleep(PAUSE).await;
    let rest = vec![b'x'; DECLARED - FIRST];
    stream
        .write_all(&rest)
        .await
        .map_err(|e| format!("writing the rest of the body after the refusal: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("flushing the body: {e}"))?;
    read_all(&mut stream).await
}

async fn read_all<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String, String> {
    let mut response = Vec::new();
    match tokio::time::timeout(READ_BUDGET, stream.read_to_end(&mut response)).await {
        Ok(Ok(_)) => Ok(String::from_utf8_lossy(&response).into_owned()),
        Ok(Err(e)) => Err(format!(
            "reading the response: {e} (after {} bytes: {:?})",
            response.len(),
            String::from_utf8_lossy(&response[..response.len().min(80)])
        )),
        Err(_) => Err(format!("no EOF within {READ_BUDGET:?}")),
    }
}

fn assert_refused(outcome: Result<String, String>, door: &str, envelope: bool) {
    let response = outcome.unwrap_or_else(|e| panic!("{door}: {e}"));
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "{door}: expected a 413, got {:?}",
        response.chars().take(200).collect::<String>()
    );
    if envelope {
        assert!(
            response.contains(r#""errors""#),
            "{door}: the 413 carries the error envelope: {response}"
        );
    }
}

async fn connect(addr: SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.expect("connect")
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[tokio::test]
async fn imposter_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let outcome = oversize_probe(connect(local(port)).await, &content_length_head("/x")).await;
    assert_refused(outcome, "imposter", true);
}

#[tokio::test]
async fn https_imposter_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let port = imposter(&manager, "https").await;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(insecure_client_config()));
    let domain = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let tls = connector
        .connect(domain, connect(local(port)).await)
        .await
        .expect("tls handshake");
    let outcome = oversize_probe(tls, &content_length_head("/x")).await;
    assert_refused(outcome, "https imposter", true);
}

/// Chunked framing has no declared length, so the refusal comes mid-stream, after the limit has
/// been read.
#[tokio::test]
async fn imposter_chunked_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let mut stream = connect(local(port)).await;
    let chunk = vec![b'x'; 64 * 1024];
    let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
    framed.extend_from_slice(&chunk);
    framed.extend_from_slice(b"\r\n");

    let outcome = async {
        stream
            .write_all(b"POST /x HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .map_err(|e| format!("writing the head: {e}"))?;
        // 2 MiB first: past the limit, so the refusal is decided while the client keeps sending.
        for _ in 0..32 {
            stream
                .write_all(&framed)
                .await
                .map_err(|e| format!("writing the first chunks: {e}"))?;
        }
        tokio::time::sleep(PAUSE).await;
        for _ in 32..128 {
            stream
                .write_all(&framed)
                .await
                .map_err(|e| format!("writing chunks after the refusal: {e}"))?;
        }
        stream
            .write_all(b"0\r\n\r\n")
            .await
            .map_err(|e| format!("writing the terminator: {e}"))?;
        read_all(&mut stream).await
    }
    .await;
    assert_refused(outcome, "imposter (chunked)", true);
}

#[tokio::test]
async fn admin_api_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let admin = start_admin(Arc::clone(&manager)).await;
    let outcome = oversize_probe(
        connect(admin.local_addr()).await,
        &content_length_head("/imposters"),
    )
    .await;
    assert_refused(outcome, "admin POST /imposters", true);
}

#[tokio::test]
async fn admin_gateway_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let admin = start_admin(Arc::clone(&manager)).await;
    let outcome = oversize_probe(
        connect(admin.local_addr()).await,
        &content_length_head(&format!("/__rift/{port}/x")),
    )
    .await;
    assert_refused(outcome, "admin /__rift/ gateway", true);
}

#[tokio::test]
async fn front_door_413_reaches_a_client_still_writing_its_body() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let front = start_front_door(Arc::clone(&manager), port).await;
    let outcome = oversize_probe(
        connect(front.local_addr()).await,
        &content_length_head("/routed/x"),
    )
    .await;
    assert_refused(outcome, "front door route", true);
}

#[tokio::test]
async fn intercept_413_reaches_a_client_still_writing_its_body() {
    let server = ServerBuilder::from_cli(Cli::parse_from([
        "rift",
        "--port",
        "0",
        "--metrics-port",
        "0",
        "--local-only",
        "--intercept-port",
        "0",
    ]))
    .start()
    .await
    .expect("server starts");
    let ca = reqwest::get(format!("http://{}/intercept/ca.pem", server.admin_addr()))
        .await
        .expect("ca.pem")
        .text()
        .await
        .expect("ca.pem body");
    let intercept = server.intercept_addr().expect("intercept bound");

    let mut stream = connect(intercept).await;
    stream
        .write_all(b"CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: cdn.example.com:443\r\n\r\n")
        .await
        .expect("CONNECT");
    let mut established = Vec::new();
    let mut byte = [0u8; 1];
    while !established.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.expect("CONNECT reply");
        assert!(n != 0, "proxy closed before answering CONNECT");
        established.push(byte[0]);
    }
    let domain = rustls::pki_types::ServerName::try_from("cdn.example.com").expect("server name");
    let tls = trusting_connector(&ca)
        .connect(domain, stream)
        .await
        .expect("tunnel handshake");

    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: cdn.example.com\r\nContent-Length: {DECLARED}\r\n\r\n"
    );
    let outcome = oversize_probe(tls, &head).await;
    // The intercept listener's refusals carry no body (issue #991), so no envelope.
    assert_refused(outcome, "intercept tunnel", false);
    server.shutdown().await;
}

/// The refusal is decided from the declared length, so the body is never wanted: no `100
/// Continue`, the `413`, then a clean EOF.
#[tokio::test]
async fn expect_continue_gets_the_413_without_a_100() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let mut stream = connect(local(port)).await;
    stream
        .write_all(
            format!(
                "POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: {DECLARED}\r\n\
                 Expect: 100-continue\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("head");
    let response = read_all(&mut stream).await.expect("413 then EOF");
    assert!(
        !response.contains("100 Continue"),
        "a refused body must not be invited: {response}"
    );
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
}

/// The drain is bounded in time: a client that never stops sending is closed at the deadline (2 s)
/// rather than held for as long as it cares to send.
#[tokio::test]
async fn a_client_that_never_stops_sending_is_closed_at_the_deadline() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let stream = connect(local(port)).await;
    let (mut reader, mut writer) = stream.into_split();

    let started = Instant::now();
    writer
        .write_all(b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1073741824\r\n\r\n")
        .await
        .expect("head");
    let read = tokio::spawn(async move {
        let mut response = Vec::new();
        let _ = reader.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    });
    // Body already in flight when the refusal lands: without the drain, the close resets at once.
    writer
        .write_all(&vec![b'x'; FIRST])
        .await
        .expect("first part of the body");

    // Slow but never idle long enough for the idle bound to end the drain first.
    let trickle = [b'x'; 1024];
    let closed_after = loop {
        if writer.write_all(&trickle).await.is_err() {
            break started.elapsed();
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("the connection was still accepting body bytes after 10 s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Both bounds: closing early is the reset this issue is about, closing late is an unbounded
    // drain.
    assert!(
        closed_after >= Duration::from_millis(1800),
        "closed after {closed_after:?}; a client still sending is drained until the 2 s deadline"
    );
    assert!(
        closed_after < Duration::from_secs(4),
        "closed after {closed_after:?}; the drain must stop at its 2 s deadline"
    );
    let response = read.await.expect("reader");
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "the 413 still arrived first: {response}"
    );
}

/// Only a refusal arms the linger. An idle keep-alive connection that was never refused must not
/// make deleting its imposter wait out the drain deadline.
#[tokio::test]
async fn an_idle_keep_alive_connection_does_not_delay_delete() {
    let manager = limited_manager();
    let port = imposter(&manager, "http").await;
    let mut stream = connect(local(port)).await;
    stream
        .write_all(b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request");
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await.expect("response");
    assert!(
        String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        "served"
    );

    let started = Instant::now();
    manager.delete_imposter(port).await.expect("delete");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "delete took {took:?} with an idle, never-refused connection open"
    );
    drop(stream);
}

fn trusting_connector(ca_pem: &str) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots
            .add(cert.expect("ca cert parses"))
            .expect("ca trusted");
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("client config")
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

fn insecure_client_config() -> rustls::ClientConfig {
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    // ALPN left empty: the probe speaks HTTP/1.1, which is the only protocol this issue concerns.
    cfg.dangerous().set_certificate_verifier(Arc::new(NoVerify));
    cfg
}
