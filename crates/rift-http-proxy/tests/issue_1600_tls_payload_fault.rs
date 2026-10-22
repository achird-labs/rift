//! Issue #1600: on an https imposter, `RANDOM_DATA_THEN_CLOSE` (and Rift's malformed-chunk fault)
//! wrote its payload on the raw socket beneath TLS, so the client's TLS layer received a
//! non-record and failed the connection with a TLS error (`ERR_SSL_BAD_RECORD_TYPE`).
//!
//! Mountebank writes the garbage on the `TLSSocket`, so it arrives as encrypted application data:
//! the client completes the handshake, decrypts the bytes and fails the HTTP parse
//! (`HPE_INVALID_CONSTANT`, mbTest `httpFaultTest.js`). These tests read the decrypted stream
//! through a real rustls client and assert the payload arrives inside TLS.
//!
//! The split is pinned too: a reset still acts on the socket beneath TLS, so the client sees a
//! connection reset, not a TLS record.

use std::sync::Arc;
use std::time::Duration;

use rift_mock_core::imposter::ImposterManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The bytes Rift sends for `RANDOM_DATA_THEN_CLOSE`, the same on http and https.
const RANDOM_DATA: &[u8] = b"\x00\xff\xfe\xfd\x13\x37\xde\xad\xbe\xef";

async fn serve(response: serde_json::Value) -> (Arc<ImposterManager>, u16) {
    let manager = Arc::new(ImposterManager::new());
    let config = serde_json::from_value(serde_json::json!({
        "protocol": "https",
        "stubs": [{ "responses": [response] }]
    }))
    .expect("imposter config");
    let port = manager
        .create_imposter(config)
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(250)).await;
    (manager, port)
}

/// What the client got after a completed handshake: the decrypted bytes, then how the stream
/// ended (`None` for a clean EOF).
struct Outcome {
    plaintext: Vec<u8>,
    end: Option<std::io::Error>,
}

/// Handshake, send one request, and read the decrypted stream until it ends. A handshake failure
/// panics: every case here must get through it, as with Mountebank.
async fn request_over_tls(port: u16) -> Outcome {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_trusting_anything()));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut tls = connector.connect(name, tcp).await.expect("tls handshake");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");
    tls.flush().await.expect("flush request");

    read_until_end(&mut tls).await
}

/// The stream must end as a dropped connection: EOF, or the unexpected EOF a missing
/// `close_notify` produces. Never a TLS record error, which is the bug.
fn assert_dropped_without_tls_error(outcome: &Outcome, case: &str) {
    if let Some(e) = &outcome.end {
        assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ),
            "{case}: the connection must end as a drop, not a TLS error: {e} ({:?})",
            e.kind()
        );
    }
}

#[tokio::test]
async fn random_data_on_https_arrives_inside_tls() {
    let (manager, port) = serve(serde_json::json!({ "fault": "RANDOM_DATA_THEN_CLOSE" })).await;

    let outcome = request_over_tls(port).await;
    assert_eq!(
        outcome.plaintext, RANDOM_DATA,
        "the garbage must be decrypted application data (stream ended with {:?})",
        outcome.end
    );
    assert_dropped_without_tls_error(&outcome, "RANDOM_DATA_THEN_CLOSE");

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn malformed_chunk_on_https_arrives_inside_tls() {
    let (manager, port) = serve(serde_json::json!({ "fault": "MALFORMED_RESPONSE_CHUNK" })).await;

    let outcome = request_over_tls(port).await;
    assert!(
        outcome.plaintext.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "the malformed response must be decrypted application data, got {:?} (stream ended \
         with {:?})",
        String::from_utf8_lossy(&outcome.plaintext),
        outcome.end
    );
    assert_dropped_without_tls_error(&outcome, "MALFORMED_RESPONSE_CHUNK");

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn a_reset_on_https_still_resets_the_socket() {
    // Passes before the fix, and pins the split: a reset acts beneath TLS (SO_LINGER 0), so the
    // client sees a reset and no decrypted bytes, as with Mountebank.
    let (manager, port) = serve(serde_json::json!({ "fault": "CONNECTION_RESET_BY_PEER" })).await;

    let outcome = request_over_tls(port).await;
    assert!(
        outcome.plaintext.is_empty(),
        "a reset sends no response bytes, got {:?}",
        String::from_utf8_lossy(&outcome.plaintext)
    );
    let kind = outcome.end.as_ref().map(std::io::Error::kind);
    assert_eq!(
        kind,
        Some(std::io::ErrorKind::ConnectionReset),
        "the client must see a reset, got {:?}",
        outcome.end
    );

    let _ = manager.delete_imposter(port).await;
}

#[tokio::test]
async fn an_empty_response_on_https_sends_nothing() {
    // Also beneath TLS: no bytes, then the connection closes.
    let (manager, port) = serve(serde_json::json!({ "fault": "EMPTY_RESPONSE" })).await;

    let outcome = request_over_tls(port).await;
    assert!(
        outcome.plaintext.is_empty(),
        "an empty response sends no bytes, got {:?}",
        String::from_utf8_lossy(&outcome.plaintext)
    );
    assert_dropped_without_tls_error(&outcome, "EMPTY_RESPONSE");

    let _ = manager.delete_imposter(port).await;
}

/// Read the decrypted stream until it ends; `None` for a clean EOF.
async fn read_until_end<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Outcome {
    let mut plaintext = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break None,
            Ok(Ok(n)) => plaintext.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => break Some(e),
            Err(_) => panic!(
                "the connection neither closed nor failed within 5 s, after {:?}",
                String::from_utf8_lossy(&plaintext)
            ),
        }
    };
    Outcome { plaintext, end }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_data_inside_a_connect_tunnel_arrives_inside_the_inner_tls() {
    // A CONNECT through an https imposter re-accepts TLS inside the tunnel (issue #1595). The
    // payload belongs to the inner request, so it must be encrypted by the inner TLS session; on
    // the outer one alone, the client's inner TLS reads it as a non-record.
    let (manager, port) = serve(serde_json::json!({ "fault": "RANDOM_DATA_THEN_CLOSE" })).await;

    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config_trusting_anything()));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut outer = connector
        .connect(name.clone(), tcp)
        .await
        .expect("outer tls handshake");
    outer
        .write_all(b"CONNECT anything.invalid:443 HTTP/1.1\r\nHost: anything.invalid:443\r\n\r\n")
        .await
        .expect("write CONNECT");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), outer.read(&mut byte))
            .await
            .expect("CONNECT response arrives")
            .expect("read CONNECT response");
        assert!(n > 0, "closed inside the CONNECT response");
        head.push(byte[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "CONNECT opens a tunnel, got {:?}",
        String::from_utf8_lossy(&head)
    );

    let mut inner = connector
        .connect(name, outer)
        .await
        .expect("inner tls handshake");
    inner
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");
    inner.flush().await.expect("flush request");
    let outcome = read_until_end(&mut inner).await;
    assert_eq!(
        outcome.plaintext, RANDOM_DATA,
        "the garbage must be the inner TLS session's application data (stream ended with {:?})",
        outcome.end
    );
    assert_dropped_without_tls_error(&outcome, "RANDOM_DATA_THEN_CLOSE in a tunnel");

    let _ = manager.delete_imposter(port).await;
}

fn client_config_trusting_anything() -> rustls::ClientConfig {
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
    cfg.dangerous().set_certificate_verifier(Arc::new(NoVerify));
    cfg
}
