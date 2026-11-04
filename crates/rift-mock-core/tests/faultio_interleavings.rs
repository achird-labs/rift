//! Connection fault injection on the real engine, on raw sockets (`formal/faultio`).
//!
//! `formal/faultio/FaultIo.tla` models how a `FaultCell` is armed, which write takes it, and how a
//! lingering close ends a connection. Each test here pins on the engine one thing the model relies
//! on, or a counterexample of it. Raw sockets on purpose: an HTTP client neither pipelines nor shows
//! which bytes arrived before a connection ended.
//!
//! The key one is the pipelining answer: a fault armed for response N+1 cannot trip on the bytes of
//! response N, because hyper reads the next head only once response N has left its write buffer
//! (`Conn::poll_flush`, `proto/h1/conn.rs:855` in hyper 1.12.0). The client below stops reading with
//! N too large for the socket buffers, so N cannot leave, and asserts the fault was not even armed.
//!
//! A test marked `#[ignore = "#n"]` states the correct behaviour and fails today: it reproduces the
//! defect filed as issue n. The fix removes the attribute.
//!
//! Ports come from the manager only (`test_port_uniqueness.rs`).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use parking_lot::Mutex;
use rift_mock_core::TcpFaultKind;
use rift_mock_core::imposter::body_limits::BodyLimits;
use rift_mock_core::imposter::{FaultLayer, ImposterManager, Point, Probe};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Far larger than loopback's socket buffers, so a client that does not read holds most of it in
/// hyper's write buffer.
const BIG: usize = 32 * 1024 * 1024;
/// How long a client stays silent to let the engine do whatever it is going to do.
const QUIET: Duration = Duration::from_millis(600);
const READ_BUDGET: Duration = Duration::from_secs(30);

const RANDOM_DATA: &[u8] = b"\x00\xff\xfe\xfd\x13\x37\xde\xad\xbe\xef";
const MALFORMED: &[u8] =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZZZZ not a valid chunk size\r\n";

#[derive(Default)]
struct Recorder(Mutex<Vec<Point>>);

impl Probe for Recorder {
    fn at(&self, point: Point) {
        self.0.lock().push(point);
    }
}

impl Recorder {
    fn armed(&self) -> Vec<TcpFaultKind> {
        self.0
            .lock()
            .iter()
            .filter_map(|p| match p {
                Point::FaultArmed { kind } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    fn taken(&self) -> Vec<(TcpFaultKind, FaultLayer)> {
        self.0
            .lock()
            .iter()
            .filter_map(|p| match p {
                Point::FaultTaken { kind, layer } => Some((*kind, *layer)),
                _ => None,
            })
            .collect()
    }

    /// The lingering-close points, in the order the engine took them.
    fn linger(&self) -> Vec<&'static str> {
        self.0
            .lock()
            .iter()
            .filter_map(|p| match p {
                Point::LingerArmed => Some("armed"),
                Point::LingerFinSent => Some("fin"),
                Point::LingerDrained { .. } => Some("drained"),
                _ => None,
            })
            .collect()
    }
}

fn path_stub(path: &str, response: Value) -> Value {
    json!({ "predicates": [{ "equals": { "path": path } }], "responses": [response] })
}

fn big_body() -> String {
    "x".repeat(BIG)
}

/// An imposter serving `stubs`, on a manager reporting to a fresh recorder.
async fn serve(
    protocol: &str,
    stubs: Vec<Value>,
    limits: Option<BodyLimits>,
) -> (Arc<ImposterManager>, u16, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let mut manager = ImposterManager::new().with_probe(Arc::clone(&recorder) as Arc<dyn Probe>);
    if let Some(limits) = limits {
        manager = manager.with_body_limits(limits);
    }
    let manager = Arc::new(manager);
    let config = serde_json::from_value(json!({ "protocol": protocol, "stubs": stubs }))
        .expect("imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    (manager, port, recorder)
}

async fn connect(port: u16) -> TcpStream {
    TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect")
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")
}

/// Everything the client received, and how the stream ended (`None`: a clean EOF).
struct Received {
    bytes: Vec<u8>,
    end: Option<std::io::Error>,
}

/// Read until the stream ends, keeping what arrived before an error.
async fn read_until_end<S: AsyncRead + Unpin>(stream: &mut S) -> Received {
    let mut bytes = Vec::new();
    let read = async {
        let mut chunk = vec![0u8; 256 * 1024];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => return None,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) => return Some(e),
            }
        }
    };
    let end = tokio::time::timeout(READ_BUDGET, read)
        .await
        .expect("the connection must end within the read budget");
    Received { bytes, end }
}

/// Split one framed response off the front of `bytes`: its status, body length, and what follows.
fn split_response(bytes: &[u8]) -> (u16, usize, &[u8]) {
    let head_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head")
        + 4;
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_lowercase();
    let status = head[9..12].parse().expect("a status");
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .expect("a content-length")
        .trim()
        .parse()
        .expect("a length");
    let end = head_end + length;
    assert!(
        bytes.len() >= end,
        "response cut short: {} of {length} body bytes arrived",
        bytes.len() - head_end
    );
    (status, length, &bytes[end..])
}

/// Pipeline `GET /big` and `GET /fault`, stay silent, and require that the fault was not armed
/// while the big response could not leave. Then read everything.
async fn pipelined_big_then_fault<S>(mut stream: S, recorder: &Recorder) -> Received
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let pipelined = format!("{}{}", get("/big"), get("/fault"));
    stream.write_all(pipelined.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");
    tokio::time::sleep(QUIET).await;
    assert!(
        recorder.armed().is_empty(),
        "response N cannot have left, so hyper must not have dispatched request N+1"
    );
    read_until_end(&mut stream).await
}

fn tls_config() -> Arc<rustls::ClientConfig> {
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
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    config
        .dangerous()
        .set_certificate_verifier(Arc::new(NoVerify));
    Arc::new(config)
}

async fn connect_tls(port: u16) -> tokio_rustls::client::TlsStream<TcpStream> {
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    tokio_rustls::TlsConnector::from(tls_config())
        .connect(name, connect(port).await)
        .await
        .expect("tls handshake")
}

fn big_and_fault(fault: &str) -> Vec<Value> {
    vec![
        path_stub("/big", json!({ "is": { "body": big_body() } })),
        path_stub("/fault", json!({ "fault": fault })),
    ]
}

/// `FaultFiresAtMostOnce`, `FaultBeforeResponseBytes` and the pipelining answer, over http: N
/// arrives whole, then the fault of N+1 and no byte of its carrier.
#[tokio::test(flavor = "multi_thread")]
async fn a_fault_for_a_pipelined_response_leaves_the_earlier_response_whole() {
    for (fault, kind) in [
        ("EMPTY_RESPONSE", TcpFaultKind::Empty),
        ("MALFORMED_RESPONSE_CHUNK", TcpFaultKind::MalformedChunk),
        ("RANDOM_DATA_THEN_CLOSE", TcpFaultKind::RandomData),
    ] {
        let (manager, port, recorder) = serve("http", big_and_fault(fault), None).await;
        let received = pipelined_big_then_fault(connect(port).await, &recorder).await;

        let (status, length, tail) = split_response(&received.bytes);
        assert_eq!((status, length), (200, BIG), "{fault}");
        assert!(received.end.is_none(), "{fault}: {:?}", received.end);
        // A payload is written with `try_write` (fault_io.rs, `trip`): after N filled the socket's
        // send buffer it may degrade to an empty close, which the code documents. It never sends
        // anything else, and never the carrier.
        let payload: &[u8] = match kind {
            TcpFaultKind::RandomData => RANDOM_DATA,
            TcpFaultKind::MalformedChunk => MALFORMED,
            _ => b"",
        };
        assert!(payload.starts_with(tail), "{fault}: tail {tail:?}");
        eprintln!(
            "{fault}: N whole, then {} of {} payload bytes",
            tail.len(),
            payload.len()
        );

        assert_eq!(recorder.armed(), vec![kind], "{fault}");
        assert_eq!(
            recorder.taken(),
            vec![(kind, FaultLayer::RawSocket)],
            "{fault}"
        );
        let _ = manager.delete_imposter(port).await;
    }
}

/// `PayloadAboveTlsOnHttps` with the pipelining answer, over https: a payload fault waits for N, is
/// taken above TLS, and arrives as decrypted application data right after N.
#[tokio::test(flavor = "multi_thread")]
async fn a_payload_fault_over_tls_follows_the_earlier_response_inside_tls() {
    for (fault, kind, payload) in [
        (
            "MALFORMED_RESPONSE_CHUNK",
            TcpFaultKind::MalformedChunk,
            MALFORMED,
        ),
        (
            "RANDOM_DATA_THEN_CLOSE",
            TcpFaultKind::RandomData,
            RANDOM_DATA,
        ),
    ] {
        let (manager, port, recorder) = serve("https", big_and_fault(fault), None).await;
        let received = pipelined_big_then_fault(connect_tls(port).await, &recorder).await;

        let (status, length, tail) = split_response(&received.bytes);
        assert_eq!((status, length), (200, BIG), "{fault}");
        assert_eq!(tail, payload, "{fault}: end {:?}", received.end);
        assert_eq!(
            recorder.taken(),
            vec![(kind, FaultLayer::AboveTls)],
            "{fault}"
        );
        let _ = manager.delete_imposter(port).await;
    }
}

/// A reset or empty close over https stays beneath TLS, in `FaultIo`, and also waits for N.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_close_over_tls_is_taken_beneath_tls_after_the_earlier_response() {
    let (manager, port, recorder) = serve("https", big_and_fault("EMPTY_RESPONSE"), None).await;
    let received = pipelined_big_then_fault(connect_tls(port).await, &recorder).await;

    let (status, length, tail) = split_response(&received.bytes);
    assert_eq!((status, length), (200, BIG));
    assert!(tail.is_empty(), "tail {tail:?}");
    assert_eq!(
        recorder.taken(),
        vec![(TcpFaultKind::Empty, FaultLayer::RawSocket)]
    );
    let _ = manager.delete_imposter(port).await;
}

/// The witness `NeverArmAfterFirstWrite`: on a kept-alive connection a fault is armed after an
/// earlier response has gone out, so a fault is not only ever the connection's first write.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_alive_connection_arms_after_its_first_response() {
    let stubs = vec![
        path_stub("/ok", json!({ "is": { "body": "ok" } })),
        path_stub("/fault", json!({ "fault": "EMPTY_RESPONSE" })),
    ];
    let (manager, port, recorder) = serve("http", stubs, None).await;
    let mut stream = connect(port).await;

    stream
        .write_all(get("/ok").as_bytes())
        .await
        .expect("write");
    let mut first = vec![0u8; 4096];
    let n = stream.read(&mut first).await.expect("first response");
    let (status, length, tail) = split_response(&first[..n]);
    assert_eq!((status, length, tail.len()), (200, 2, 0));
    assert!(
        recorder.armed().is_empty(),
        "armed only by the second request"
    );

    stream
        .write_all(get("/fault").as_bytes())
        .await
        .expect("write");
    let received = read_until_end(&mut stream).await;
    assert!(received.bytes.is_empty(), "{:?}", received.bytes);
    assert_eq!(recorder.armed(), vec![TcpFaultKind::Empty]);
    assert_eq!(recorder.taken().len(), 1, "fires once");
    let _ = manager.delete_imposter(port).await;
}

/// Pipeline `/big` and a reset on a socket with a small receive window, reading until the fault
/// fires with N's tail still queued in the server's kernel. Returns how many bytes of N arrived.
async fn bytes_of_n_before_a_reset() -> usize {
    let (manager, port, recorder) =
        serve("http", big_and_fault("CONNECTION_RESET_BY_PEER"), None).await;
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket.set_recv_buffer_size(4096).expect("buffer size");
    let mut stream = socket
        .connect(([127, 0, 0, 1], port).into())
        .await
        .expect("connect");
    let pipelined = format!("{}{}", get("/big"), get("/fault"));
    stream.write_all(pipelined.as_bytes()).await.expect("write");
    tokio::time::sleep(QUIET).await;
    assert!(recorder.armed().is_empty(), "N+1 waits for N");

    let mut early = Vec::new();
    let mut chunk = vec![0u8; 4 * 1024];
    while recorder.taken().is_empty() {
        // Near the end, read slowly: the server must get to the fault with the tail still queued.
        if early.len() + 512 * 1024 >= BIG {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !recorder.taken().is_empty() {
                break;
            }
        }
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => early.extend_from_slice(&chunk[..n]),
        }
    }
    eprintln!("the fault fired with {} bytes read", early.len());
    tokio::time::sleep(QUIET).await;
    let mut received = read_until_end(&mut stream).await;
    early.append(&mut received.bytes);

    let head_end = early
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head")
        + 4;
    let body = &early[head_end..];
    assert!(body.iter().all(|b| *b == b'x'), "only N's bytes");
    eprintln!(
        "reset: {} of {BIG} bytes of N arrived, then {:?}",
        body.len(),
        received.end.as_ref().map(std::io::Error::kind)
    );
    assert_eq!(
        recorder.taken(),
        vec![(TcpFaultKind::Reset, FaultLayer::RawSocket)]
    );
    let _ = manager.delete_imposter(port).await;
    body.len()
}

/// A reset for N+1 sends nothing of its own: what arrives is a prefix of N, never the carrier.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_for_a_pipelined_response_adds_nothing_to_the_earlier_one() {
    assert!(bytes_of_n_before_a_reset().await <= BIG);
}

/// `RstDiscardsNothing`: hyper hands N to the kernel before N+1 runs, but `SO_LINGER(0)` discards
/// what the kernel has not sent yet, so a client that pipelines past a slow read loses N's tail.
/// Depends on the kernel's buffers; reproduced on macOS loopback.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "#1783"]
async fn a_reset_for_a_pipelined_response_leaves_the_earlier_response_whole() {
    assert_eq!(bytes_of_n_before_a_reset().await, BIG);
}

/// The documented limit `GracefulFaultEndsInFin`: bytes the client pipelined past the faulted
/// request sit unread in the kernel when the socket closes, so a graceful fault reaches the client
/// as a reset. The kernel decides that, as it did for the refusals #1460 lingers on.
#[tokio::test(flavor = "multi_thread")]
async fn unread_pipelined_bytes_turn_an_empty_close_into_a_reset() {
    let stubs = vec![path_stub("/fault", json!({ "fault": "EMPTY_RESPONSE" }))];
    let (manager, port, recorder) = serve("http", stubs, None).await;
    let mut stream = connect(port).await;

    let mut pipelined = get("/fault").into_bytes();
    pipelined.extend(std::iter::repeat_n(b'j', 2 * 1024 * 1024));
    // The kernel may refuse the tail once the connection is gone; that is the point, not a failure.
    let _ = stream.write_all(&pipelined).await;
    let received = read_until_end(&mut stream).await;
    assert!(received.bytes.is_empty());
    eprintln!(
        "empty close with unread bytes: {:?}",
        received.end.as_ref().map(std::io::Error::kind)
    );
    assert_eq!(recorder.taken().len(), 1);
    let _ = manager.delete_imposter(port).await;
}

fn oversize_head(declared: usize) -> String {
    format!("POST /ok HTTP/1.1\r\nHost: localhost\r\nContent-Length: {declared}\r\n\r\n")
}

/// `LingerDeliversResponse`: the refusal, then a FIN, then the drain, while the client still writes.
async fn refusal_reaches_a_client_still_writing<S>(mut stream: S, recorder: &Recorder)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const LIMIT: usize = 1024 * 1024;
    let declared = 4 * LIMIT;
    stream
        .write_all(oversize_head(declared).as_bytes())
        .await
        .expect("head");
    stream
        .write_all(&vec![b'x'; 256 * 1024])
        .await
        .expect("first");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stream
        .write_all(&vec![b'x'; declared - 256 * 1024])
        .await
        .expect("the rest of the body is drained, not refused");
    stream.flush().await.expect("flush");
    let received = read_until_end(&mut stream).await;
    let (status, _, tail) = split_response(&received.bytes);
    assert_eq!(status, 413);
    assert!(tail.is_empty());
    assert!(
        recorder.linger().starts_with(&["armed", "fin"]),
        "{:?}",
        recorder.linger()
    );
}

fn limited() -> Option<BodyLimits> {
    Some(BodyLimits::new(1024 * 1024, None).expect("limits"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lingering_close_delivers_the_refusal_over_http() {
    let stubs = vec![path_stub("/ok", json!({ "is": { "body": "ok" } }))];
    let (manager, port, recorder) = serve("http", stubs, limited()).await;
    refusal_reaches_a_client_still_writing(connect(port).await, &recorder).await;
    let _ = manager.delete_imposter(port).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lingering_close_delivers_the_refusal_over_https() {
    let stubs = vec![path_stub("/ok", json!({ "is": { "body": "ok" } }))];
    let (manager, port, recorder) = serve("https", stubs, limited()).await;
    refusal_reaches_a_client_still_writing(connect_tls(port).await, &recorder).await;
    let _ = manager.delete_imposter(port).await;
}

/// `LingerDeliversResponse` when an earlier pipelined response has filled the socket: the refusal
/// must wait its turn in hyper's write buffer, and `Buffered::poll_shutdown` flushes it before the
/// FIN (`proto/h1/io.rs:337`), even though both sides of the connection already count as closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_behind_a_blocked_response_still_arrives_before_the_fin() {
    let stubs = vec![
        path_stub("/big", json!({ "is": { "body": big_body() } })),
        path_stub("/ok", json!({ "is": { "body": "ok" } })),
    ];
    let (manager, port, recorder) = serve("http", stubs, limited()).await;
    let mut stream = connect(port).await;

    let mut pipelined = get("/big").into_bytes();
    pipelined.extend_from_slice(oversize_head(4 * 1024 * 1024).as_bytes());
    pipelined.extend_from_slice(&vec![b'x'; 16 * 1024]);
    stream.write_all(&pipelined).await.expect("write");
    tokio::time::sleep(QUIET).await;
    assert!(
        recorder.linger().is_empty(),
        "request N+1 waits for response N: {:?}",
        recorder.linger()
    );

    let received = read_until_end(&mut stream).await;
    let (status, length, tail) = split_response(&received.bytes);
    assert_eq!((status, length), (200, BIG));
    let (status, _, rest) = split_response(tail);
    assert_eq!(status, 413);
    assert!(rest.is_empty());
    assert!(received.end.is_none(), "{:?}", received.end);
    assert_eq!(recorder.linger()[..2], ["armed", "fin"]);
    let _ = manager.delete_imposter(port).await;
}

async fn h2_status(
    sender: &mut hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
    port: u16,
    path: &str,
) -> Result<u16, String> {
    let request = hyper::Request::get(format!("http://127.0.0.1:{port}{path}"))
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("request: {e}"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let status = response.status().as_u16();
    response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("body: {e:?}"))?;
    Ok(status)
}

/// A connection is served HTTP/1-only when the imposter can fault at accept time
/// (`manager.rs`, `http1_only`); a fault stub added afterwards reaches an HTTP/2 connection already
/// open. The fault of one stream must then reset that stream, as `apply_tcp_fault` does on the
/// other listeners, and leave the connection's other streams running.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "#1782"]
async fn a_fault_stub_added_under_an_open_h2_connection_resets_only_its_stream() {
    let stubs = vec![
        path_stub("/ok", json!({ "is": { "body": "ok" } })),
        path_stub(
            "/slow",
            json!({ "is": { "body": "slow" }, "_behaviors": { "wait": 1500 } }),
        ),
    ];
    let (manager, port, recorder) = serve("http", stubs, None).await;

    let (mut sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(connect(port).await),
    )
    .await
    .expect("h2 handshake");
    let driver = tokio::spawn(conn);
    assert_eq!(h2_status(&mut sender, port, "/ok").await, Ok(200));

    let stub = serde_json::from_value(path_stub("/fault", json!({ "fault": "EMPTY_RESPONSE" })))
        .expect("stub");
    manager
        .add_stub(port, stub, Some(0))
        .await
        .expect("add stub");

    let mut slow_sender = sender.clone();
    let slow = tokio::spawn(async move { h2_status(&mut slow_sender, port, "/slow").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let faulted = h2_status(&mut sender, port, "/fault").await;
    assert!(
        faulted.is_err(),
        "the faulted stream ends in a reset: {faulted:?}"
    );
    let slow = slow.await.expect("task");
    assert_eq!(
        slow,
        Ok(200),
        "the other stream must survive; armed {:?}, taken {:?}",
        recorder.armed(),
        recorder.taken()
    );
    assert!(
        recorder.armed().is_empty(),
        "h2 must not arm the connection's cell"
    );
    driver.abort();
    let _ = manager.delete_imposter(port).await;
}
