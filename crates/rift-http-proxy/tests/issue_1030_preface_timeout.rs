//! Issue #1030: a connection that completes the transport handshake and then sends nothing — or
//! sends a partial HTTP/2 preface and stops — was never timed out on the `auto::Builder` path.
//!
//! `auto::Builder` sits in `ReadVersion` sniffing the protocol preface before it constructs the
//! HTTP/1 connection, and only *that* connection carries `header_read_timeout`. So the detection
//! window itself had no deadline: the socket, its task and its `TlsStream` were pinned for as long
//! as the client cared to stay quiet, unauthenticated.
//!
//! These drive real listeners over real sockets, because the defect is in how a listener wires
//! detection to its timer — a unit test on the sniffer cannot show that the listener uses it.
//!
//! This is its own test binary so `RIFT_HTTP_HEADER_TIMEOUT` can be set process-wide (the knob is
//! read once per listener at bind time) without perturbing any other suite.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_mock_core::imposter::ImposterManager;

/// The deadline every test here relies on. Set before any listener binds.
const HEADER_TIMEOUT_SECS: u64 = 1;

/// Generous enough to absorb CI scheduling noise, tight enough that "never closes" still fails:
/// the pre-fix behaviour is unbounded, so any finite bound discriminates.
const CLOSE_BUDGET: Duration = Duration::from_secs(8);

/// SAFETY: `set_var` races any concurrent `getenv`, and binding a listener calls
/// `HttpTuning::from_env()` — so this is only sound because every test in this binary is
/// `#[serial_test::serial]`, leaving no other thread reading the environment while this writes it.
/// The serial attribute is load-bearing, not tidiness: without it four other listeners in this
/// same binary bind concurrently and the race is real UB, not theoretical.
fn init_env() {
    unsafe { std::env::set_var("RIFT_HTTP_HEADER_TIMEOUT", HEADER_TIMEOUT_SECS.to_string()) };
}

async fn serve(config: serde_json::Value) -> Arc<ImposterManager> {
    init_env();
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(serde_json::from_value(config).expect("imposter config"))
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(250)).await;
    manager
}

/// Connect, optionally send `probe`, then read until the server closes. Returns how long the
/// server took to close, or `None` if it never did within `CLOSE_BUDGET`.
///
/// A closed connection surfaces as `read` returning 0 (or an error); a *parked* one blocks until
/// the read timeout expires, which is what the pre-fix server does.
fn time_until_close(port: u16, probe: &[u8]) -> Option<Duration> {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(CLOSE_BUDGET))
        .expect("set read timeout");
    let started = Instant::now();
    if !probe.is_empty() {
        let mut s = &stream;
        s.write_all(probe).expect("write probe");
        s.flush().ok();
    }
    let mut sink = [0u8; 64];
    let mut s = &stream;
    match s.read(&mut sink) {
        Ok(0) => Some(started.elapsed()),
        // Any server-side close (RST included) counts: the point is that the socket did not stay
        // pinned. A read that returns data would mean the server answered, which these probes
        // never warrant.
        Ok(_) => None,
        Err(_) => {
            let elapsed = started.elapsed();
            // A timeout of our own read budget means the server never closed — the bug.
            if elapsed >= CLOSE_BUDGET {
                None
            } else {
                Some(elapsed)
            }
        }
    }
}

/// Assert the connection was closed *by the deadline*, not merely closed.
///
/// Without a lower bound, a server that dropped every new connection instantly — a panicking
/// connection task, or a fix that closed too eagerly — would satisfy "it closed" just as well as
/// the correct behaviour. The floor is deliberately well under the configured 1s so ordinary
/// scheduling jitter cannot fail it, while still ruling out an instant teardown.
fn assert_closed_by_the_deadline(closed: Option<Duration>, what: &str) {
    let elapsed = closed.unwrap_or_else(|| {
        panic!("{what} was never closed within {CLOSE_BUDGET:?}; it stayed pinned")
    });
    assert!(
        elapsed >= Duration::from_millis(600),
        "{what} closed after only {elapsed:?} — that is too fast to have come from the \
         {HEADER_TIMEOUT_SECS}s detection deadline, so something else is tearing connections down"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn a_plaintext_imposter_closes_a_connection_that_sends_nothing() {
    // THE BUG on the cheapest listener to reach. Pre-fix this connection is held forever.
    let manager = serve(serde_json::json!({
        "port": 21540, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
    }))
    .await;

    let closed = tokio::task::spawn_blocking(|| time_until_close(21540, b""))
        .await
        .expect("probe task");

    assert_closed_by_the_deadline(closed, "a client that sent nothing");

    let _ = manager.delete_imposter(21540).await;
}

// Issue #1045: the counter must actually be wired to the listener, and must discriminate the
// cause. A unit test on `record_preface_failure` proves the mapping but not that any listener
// calls it — which is the half that would silently regress, exactly as the sniffer's own unit
// tests could not show that a listener wired detection to its timer (#1030, above).
//
// Counters are process-global and this binary is `#[serial]`, so both assertions are deltas.
#[tokio::test]
#[serial_test::serial]
async fn a_dropped_connection_is_counted_under_the_kind_that_caused_it() {
    let manager = serve(serde_json::json!({
        "port": 21549, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
    }))
    .await;

    let timeouts_before = preface_failures("imposter", "timeout");
    let eofs_before = preface_failures("imposter", "eof");

    // Connect and stay silent -> the sniffer's deadline expires -> `Timeout`.
    let _ = tokio::task::spawn_blocking(|| time_until_close(21549, b""))
        .await
        .expect("silent probe");
    // Connect and hang up immediately -> the read returns 0 bytes -> `Eof`.
    tokio::task::spawn_blocking(|| {
        drop(TcpStream::connect(("127.0.0.1", 21549)).expect("connect"));
    })
    .await
    .expect("eof probe");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        preface_failures("imposter", "timeout") > timeouts_before,
        "a client that connected and went quiet must land on kind=timeout"
    );
    assert!(
        preface_failures("imposter", "eof") > eofs_before,
        "a client that hung up must land on kind=eof, NOT on timeout — telling those two apart \
         from a systemic `io` rate is the entire reason this metric exists"
    );

    let _ = manager.delete_imposter(21549).await;
}

/// Read one child of `rift_preface_failures_total` out of the scrape.
fn preface_failures(listener: &str, kind: &str) -> f64 {
    let needle = format!(r#"rift_preface_failures_total{{kind="{kind}",listener="{listener}"}} "#);
    rift_mock_core::extensions::metrics::collect_metrics()
        .lines()
        .find_map(|l| l.strip_prefix(needle.as_str()))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or_else(|| {
            panic!("series {needle}not found — it must be materialised at 0 from listener start")
        })
}

#[tokio::test]
#[serial_test::serial]
async fn a_plaintext_imposter_closes_a_connection_stuck_mid_preface() {
    // The correction the triage made to the issue's own framing: the detection loop exits only on
    // a complete 24-byte preface or a diverging byte, so `P` — which *matches* the preface's first
    // byte — leaves the connection just as stuck as sending nothing. A fix keyed on "has the
    // client sent any bytes?" would pass the test above and fail this one.
    let manager = serve(serde_json::json!({
        "port": 21541, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
    }))
    .await;

    let closed = tokio::task::spawn_blocking(|| time_until_close(21541, b"P"))
        .await
        .expect("probe task");

    assert_closed_by_the_deadline(closed, "a client stuck mid-preface");

    let _ = manager.delete_imposter(21541).await;
}

#[tokio::test]
#[serial_test::serial]
async fn an_https_imposter_closes_a_connection_that_sends_nothing_after_the_handshake() {
    // The case the issue is titled for. The TLS handshake itself is already bounded; the
    // unbounded window starts immediately after it, and a parked TLS connection costs more than a
    // plaintext one (a whole `TlsStream` plus session state).
    let manager = serve(serde_json::json!({
        "port": 21542, "protocol": "https",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "ok"}}]}]
    }))
    .await;

    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", 21542))
        .await
        .expect("connect");
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(insecure_client_config()));
    let domain = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut tls = connector.connect(domain, tcp).await.expect("tls handshake");

    // Handshake done, then nothing. Pre-fix this read blocks forever.
    let started = Instant::now();
    let mut sink = [0u8; 64];
    let outcome = tokio::time::timeout(
        CLOSE_BUDGET,
        tokio::io::AsyncReadExt::read(&mut tls, &mut sink),
    )
    .await;

    assert!(
        outcome.is_ok(),
        "an HTTPS imposter must close a connection that completes the TLS handshake and then \
         sends nothing; it was still open after {CLOSE_BUDGET:?}"
    );
    assert!(
        started.elapsed() < CLOSE_BUDGET,
        "closed, but not within the budget"
    );

    let _ = manager.delete_imposter(21542).await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_slow_but_legitimate_client_is_still_served() {
    // The companion that passes before the fix too. Without it, "close everything quickly" would
    // satisfy every assertion above while breaking real clients — the deadline must bound silence,
    // not latency.
    let manager = serve(serde_json::json!({
        "port": 21543, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "served"}}]}]
    }))
    .await;

    let body = tokio::task::spawn_blocking(|| {
        let mut stream = TcpStream::connect(("127.0.0.1", 21543)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        // Well inside the 1s deadline, but not instant: a client that pauses before its first
        // byte is ordinary, not hostile. Kept to a quarter of the deadline rather than half so a
        // loaded CI runner cannot turn a correct server into a red test.
        std::thread::sleep(Duration::from_millis(250));
        stream
            .write_all(b"GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write");
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    })
    .await
    .expect("probe task");

    assert!(
        body.contains("served"),
        "a client that pauses and then sends a real request must still be served, got: {body}"
    );

    let _ = manager.delete_imposter(21543).await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_request_arriving_immediately_is_unaffected() {
    // Guards the replay path: detection consumes bytes off the socket, and if they were not handed
    // back byte-exact the very first request on every connection would break. That failure would
    // be catastrophic and obvious — which is exactly why it deserves a cheap pin rather than trust.
    let manager = serve(serde_json::json!({
        "port": 21544, "protocol": "http",
        "stubs": [{"predicates": [{"equals": {"path": "/replayed"}}],
                   "responses": [{"is": {"statusCode": 200, "body": "intact"}}]}]
    }))
    .await;

    let body = reqwest::Client::new()
        .get("http://127.0.0.1:21544/replayed")
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert_eq!(
        body, "intact",
        "the sniffed bytes must be replayed byte-exact, or the first request on every connection \
         is corrupted"
    );

    let _ = manager.delete_imposter(21544).await;
}

/// The imposter serves a self-signed certificate, so the client must not verify it — this is a
/// test of connection lifetime, not of trust.
#[tokio::test]
#[serial_test::serial]
async fn an_established_keep_alive_connection_survives_past_the_deadline() {
    // THE REGRESSION GUARD. The obvious fix for #1030 — wrapping `serve_connection` in a timeout —
    // would bound the whole connection rather than just its detection window, silently closing
    // every keep-alive connection at the deadline. Nothing else in this suite would notice: all
    // the other tests here want connections to close.
    //
    // So: establish a connection, serve a request on it, let it sit idle for longer than the
    // detection deadline, then serve a second request on the SAME connection. The pool reuses it,
    // so a second success means it was never torn down.
    let manager = serve(serde_json::json!({
        "port": 21545, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "alive"}}]}]
    }))
    .await;

    // `pool_max_idle_per_host` default keeps the connection; disabling `Connection: close` is
    // implicit for HTTP/1.1 keep-alive.
    let client = reqwest::Client::builder()
        .pool_idle_timeout(None)
        .build()
        .expect("client");

    let first = client
        .get("http://127.0.0.1:21545/one")
        .send()
        .await
        .expect("first request")
        .text()
        .await
        .expect("first body");
    assert_eq!(first, "alive");

    // Comfortably past the 1s detection deadline — and past 2x it, the documented worst case.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let second = client
        .get("http://127.0.0.1:21545/two")
        .send()
        .await
        .expect(
            "a second request on an idle-but-established connection must still be served — if \
             this fails, the deadline is bounding the whole connection instead of just its \
             protocol-detection window",
        )
        .text()
        .await
        .expect("second body");
    assert_eq!(second, "alive");

    let _ = manager.delete_imposter(21545).await;
}

#[tokio::test]
#[serial_test::serial]
async fn an_established_h2_connection_survives_past_the_deadline() {
    // The same guard over HTTP/2, which is where it matters most: an h2 connection is expected to
    // be long-lived and multiplexed, and it reaches the server through the very preface sniff this
    // change adds — a full 24-byte preface, the one input that makes the sniffer read to its
    // maximum length before resolving.
    let manager = serve(serde_json::json!({
        "port": 21546, "protocol": "http",
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "h2-alive"}}]}]
    }))
    .await;

    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .pool_idle_timeout(None)
        .build()
        .expect("h2 client");

    let first = client
        .get("http://127.0.0.1:21546/one")
        .send()
        .await
        .expect("first h2 request")
        .text()
        .await
        .expect("first body");
    assert_eq!(first, "h2-alive");

    tokio::time::sleep(Duration::from_secs(3)).await;

    let second = client
        .get("http://127.0.0.1:21546/two")
        .send()
        .await
        .expect("an idle h2 connection must survive past the detection deadline")
        .text()
        .await
        .expect("second body");
    assert_eq!(second, "h2-alive");

    let _ = manager.delete_imposter(21546).await;
}

#[tokio::test]
#[serial_test::serial]
async fn the_intercept_listener_closes_a_tunnel_that_sends_no_request() {
    // The intercept listener is the fifth `auto::Builder` site and the one whose wiring is least
    // like the others (its own shutdown channel, its own semaphore). The pre-existing
    // `shutdown_closes_a_tunnel_awaiting_its_first_request` proves an idle tunnel dies when
    // shutdown is *called*; this proves it dies on its own once the deadline passes, which is the
    // actual subject of #1030.
    init_env();
    let ca = rift_mock_core::proxy::intercept_ca::CertificateAuthority::generate().expect("ca");
    let ca_pem = ca.ca_cert_pem().to_string();
    let resolver = std::sync::Arc::new(rift_mock_core::proxy::intercept_ca::SniCertResolver::new(
        std::sync::Arc::new(ca),
    ));
    let listener = rift_http_proxy::intercept::InterceptListener::bind(
        "127.0.0.1:0".parse().expect("addr"),
        resolver,
        rift_http_proxy::intercept_rules::InterceptRules::new(),
        None,
    )
    .await
    .expect("bind intercept listener");
    let addr = listener.local_addr();

    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::io::AsyncWriteExt::write_all(&mut sock, b"CONNECT x.example.com:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("CONNECT");
    let mut head = [0u8; 128];
    let n = tokio::io::AsyncReadExt::read(&mut sock, &mut head)
        .await
        .expect("CONNECT response");
    assert!(
        String::from_utf8_lossy(&head[..n]).starts_with("HTTP/1.1 200"),
        "CONNECT must be accepted before the tunnel exists"
    );

    // The TLS handshake is what actually puts the connection inside `serve_tunnel`, where the
    // preface sniff runs. Stopping at the CONNECT response would leave it in the handshake phase
    // instead — which is bounded by a different timeout and is not what #1030 is about.
    let server_name =
        rustls::pki_types::ServerName::try_from("x.example.com").expect("server name");
    let mut tls = trusting_connector(&ca_pem)
        .connect(server_name, sock)
        .await
        .expect("client handshake against the minted leaf");

    // Tunnel established, and now the client says nothing at all. Pre-fix this socket is held
    // until the process ends.
    let mut byte = [0u8; 1];
    let outcome = tokio::time::timeout(
        CLOSE_BUDGET,
        tokio::io::AsyncReadExt::read(&mut tls, &mut byte),
    )
    .await;

    assert!(
        outcome.is_ok(),
        "an intercept tunnel whose client sends no request must be closed within \
         {CLOSE_BUDGET:?}, not held open indefinitely"
    );

    listener.shutdown().await;
}

/// A TLS client that trusts the listener's freshly-minted CA — the tunnel serves a leaf signed by
/// it, so an untrusting client would fail the handshake and never reach the phase under test.
fn trusting_connector(ca_pem: &str) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots
            .add(cert.expect("ca cert parses"))
            .expect("ca trusted");
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("client config")
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
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

    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    cfg.dangerous()
        .set_certificate_verifier(std::sync::Arc::new(NoVerify));
    cfg
}
