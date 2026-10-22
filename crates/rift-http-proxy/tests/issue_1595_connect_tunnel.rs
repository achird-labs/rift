//! Issue #1595: an imposter answers `CONNECT` with a tunnel back into itself, as Mountebank does.
//!
//! Mountebank (`baseHttpServer.js`, the `connect` event) ignores the CONNECT target, answers
//! `200 Connection established` and pipes the client into its own port, so the client's inner
//! request is matched against the same imposter's stubs. The CONNECT itself is never matched,
//! recorded or counted. Rift matched the CONNECT against the stubs instead, and a `proxy` stub
//! forwarded it upstream as `CONNECT <proxy.to>`.
//!
//! Every imposter takes an engine-assigned port.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

async fn create(manager: &ImposterManager, protocol: &str) -> u16 {
    let cfg: Value = json!({
        "protocol": protocol,
        "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "inner"}}]}]
    });
    let port = manager
        .create_imposter(serde_json::from_value(cfg).expect("config"))
        .await
        .expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// Send `CONNECT` for a host that does not exist and read the response head byte by byte, so no
/// byte that belongs to the tunnel is consumed.
async fn connect<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> String {
    stream
        .write_all(b"CONNECT anything.invalid:443 HTTP/1.1\r\nHost: anything.invalid:443\r\n\r\n")
        .await
        .expect("write CONNECT");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("CONNECT response arrives")
            .expect("read");
        assert!(
            n > 0,
            "closed inside the CONNECT response: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// `GET /x` with `Connection: close`, read to EOF.
async fn get_to_eof<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> String {
    stream
        .write_all(b"GET /x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write GET");
    let mut out = Vec::new();
    // A TLS peer that closes without close_notify reports an error after the bytes arrived; the
    // bytes read so far are what the assertion needs.
    let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut out))
        .await
        .expect("inner response arrives");
    String::from_utf8_lossy(&out).into_owned()
}

fn tls_connector(alpn: &[&[u8]]) -> tokio_rustls::TlsConnector {
    let mut cfg = client_config_trusting_anything();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

fn server_name() -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from("localhost").expect("server name")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_https_imposter_tunnels_connect_back_into_itself() {
    let manager = ImposterManager::new();
    let port = create(&manager, "https").await;

    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    // A proxy connection speaks HTTP/1: CONNECT tunnels are an HTTP/1 mechanism here.
    let mut outer = tls_connector(&[b"http/1.1"])
        .connect(server_name(), tcp)
        .await
        .expect("TLS to the proxy imposter");

    let head = connect(&mut outer).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "CONNECT is answered 200, got {head:?}"
    );
    assert!(
        !head.to_ascii_lowercase().contains("x-rift-"),
        "the tunnel response is not decorated: {head:?}"
    );

    // The inner TLS handshake lands on the same imposter and gets its certificate.
    let mut inner = tls_connector(&[b"http/1.1"])
        .connect(server_name(), outer)
        .await
        .expect("TLS inside the tunnel");
    let response = get_to_eof(&mut inner).await;
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("inner"),
        "the tunnelled request is answered by the imposter's stub, got {response:?}"
    );

    let imposter = manager.get_imposter(port).expect("imposter");
    assert_eq!(
        imposter.get_request_count(),
        1,
        "only the tunnelled request is counted, not the CONNECT"
    );
    manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_imposter_tunnels_connect_back_into_itself() {
    let manager = ImposterManager::new();
    let port = create(&manager, "http").await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let head = connect(&mut stream).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "CONNECT is answered 200, got {head:?}"
    );
    let response = get_to_eof(&mut stream).await;
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("inner"),
        "the tunnelled request is answered by the imposter's stub, got {response:?}"
    );

    let imposter = manager.get_imposter(port).expect("imposter");
    assert_eq!(
        imposter.get_request_count(),
        1,
        "only the tunnelled request is counted, not the CONNECT"
    );
    manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_the_imposter_closes_an_open_tunnel() {
    let manager = ImposterManager::new();
    let port = create(&manager, "http").await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let head = connect(&mut stream).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "CONNECT is answered 200, got {head:?}"
    );
    // One keep-alive request through the tunnel, so the inner connection is live and idle.
    stream
        .write_all(b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write GET");
    let mut text = String::new();
    let mut buf = [0u8; 1024];
    while !text.ends_with("inner") {
        let n = timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("inner response arrives")
            .expect("read");
        assert!(n > 0, "tunnel closed before the response: {text:?}");
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
    }

    let started = Instant::now();
    manager.delete_imposter(port).await.expect("delete");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "delete closes the tunnel instead of waiting out the drain; took {took:?}"
    );
    let mut rest = [0u8; 16];
    match timeout(Duration::from_secs(2), stream.read(&mut rest)).await {
        Ok(Ok(0) | Err(_)) => {}
        Ok(Ok(n)) => panic!(
            "unexpected bytes after delete: {:?}",
            String::from_utf8_lossy(&rest[..n])
        ),
        Err(_) => panic!("the tunnel is still open 2s after its imposter was deleted"),
    }
}

// Each tunnel nested in another adds a layer to the connection, so nesting is bounded (eight
// levels); the CONNECT past the bound takes the ordinary request path and reaches the stub.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_tunnels_are_bounded() {
    let manager = ImposterManager::new();
    let port = create(&manager, "http").await;

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    for level in 1..=8 {
        let head = connect(&mut stream).await;
        assert!(
            head.starts_with("HTTP/1.1 200") && !head.to_ascii_lowercase().contains("x-rift-"),
            "tunnel {level} opens, got {head:?}"
        );
    }
    let head = connect(&mut stream).await;
    assert!(
        head.to_ascii_lowercase().contains("x-rift-imposter"),
        "the ninth nested CONNECT is answered by the stub, got {head:?}"
    );
    let imposter = manager.get_imposter(port).expect("imposter");
    assert_eq!(
        imposter.get_request_count(),
        1,
        "only that CONNECT is counted"
    );
    manager.delete_all().await;
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
