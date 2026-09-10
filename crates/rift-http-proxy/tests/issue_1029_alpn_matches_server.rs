//! Issue #1029: an HTTPS imposter advertised `h2` in its ALPN offer unconditionally, while the
//! server serves HTTP/1-only whenever the imposter can fire a TCP fault, runs a `_rift.script`
//! response, or `RIFT_DISABLE_HTTP2` is set.
//!
//! Advertising a protocol the server will not speak is worse than not advertising it: ALPN is
//! negotiated during the handshake, so a client that selects `h2` has already committed by the
//! time the server answers in HTTP/1, and has no way back.
//!
//! These assert the **negotiated** protocol from a real TLS handshake, because that is the thing
//! the client commits to — a unit test on the offer cannot show what a client actually ends up
//! speaking.

use std::sync::Arc;
use std::time::Duration;

use rift_mock_core::imposter::ImposterManager;

/// Complete a TLS handshake offering both protocols and report what was negotiated.
///
/// `None` means the server offered nothing / the handshake produced no ALPN, which is itself a
/// distinguishable outcome and never silently reported as success.
async fn negotiated_alpn(port: u16) -> Option<String> {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut cfg = client_config_trusting_anything();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let tls = connector.connect(name, tcp).await.expect("tls handshake");
    let (_, conn) = tls.get_ref();
    conn.alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).to_string())
}

fn fault_stub() -> serde_json::Value {
    // A TCP fault is meaningless under h2 multiplexing — one stream's fault would tear down every
    // concurrent stream — so an imposter that can fire one is served HTTP/1-only.
    serde_json::json!({"responses": [{"fault": "CONNECTION_RESET_BY_PEER"}]})
}

fn plain_stub() -> serde_json::Value {
    serde_json::json!({"responses": [{"is": {"statusCode": 200, "body": "ok"}}]})
}

async fn serve(config: serde_json::Value) -> Arc<ImposterManager> {
    let manager = Arc::new(ImposterManager::new());
    manager
        .create_imposter(serde_json::from_value(config).expect("imposter config"))
        .await
        .expect("create imposter");
    tokio::time::sleep(Duration::from_millis(250)).await;
    manager
}

#[tokio::test]
async fn a_fault_stub_imposter_does_not_advertise_h2() {
    // THE BUG. Today the handshake offers h2, the client selects it, and the server then speaks
    // HTTP/1 at it.
    let manager = serve(serde_json::json!({
        "port": 21550, "protocol": "https", "stubs": [fault_stub()]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21550).await.as_deref(),
        Some("http/1.1"),
        "an imposter that is served HTTP/1-only must not offer h2 — a client that selects it has \
         already committed by the end of the handshake"
    );

    let _ = manager.delete_imposter(21550).await;
}

#[tokio::test]
async fn an_ordinary_https_imposter_still_negotiates_h2() {
    // Passes before the fix, and is here for that reason: without it, "never advertise h2" would
    // satisfy the test above while silently disabling HTTP/2 for every HTTPS imposter.
    let manager = serve(serde_json::json!({
        "port": 21551, "protocol": "https", "stubs": [plain_stub()]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21551).await.as_deref(),
        Some("h2"),
        "an imposter with no fault and no script must still negotiate HTTP/2"
    );

    let _ = manager.delete_imposter(21551).await;
}

#[tokio::test]
async fn the_advertisement_follows_a_stub_mutation_in_both_directions() {
    // THE CASE A BUILD-TIME FIX CANNOT PASS. The TLS acceptor is built once per imposter, but
    // whether the server is HTTP/1-only depends on the *live* stub set, which changes through the
    // admin API without rebuilding the acceptor. So threading the flag in at construction — the
    // fix the issue proposes — is correct only until the first stub mutation, and then wrong again
    // in whichever direction the mutation went.
    let manager = serve(serde_json::json!({
        "port": 21552, "protocol": "https", "stubs": [plain_stub()]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21552).await.as_deref(),
        Some("h2"),
        "baseline: a plain stub negotiates h2"
    );

    // Add a fault: the very next connection must stop advertising h2.
    manager
        .replace_stubs(
            21552,
            serde_json::from_value(serde_json::json!([fault_stub()])).expect("stubs"),
        )
        .await
        .expect("replace stubs with a fault stub");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        negotiated_alpn(21552).await.as_deref(),
        Some("http/1.1"),
        "after a fault stub is added, the offer must follow — the acceptor was built before this \
         mutation existed"
    );

    // And back again: the flip is not one-way.
    manager
        .replace_stubs(
            21552,
            serde_json::from_value(serde_json::json!([plain_stub()])).expect("stubs"),
        )
        .await
        .expect("replace stubs back with a plain stub");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        negotiated_alpn(21552).await.as_deref(),
        Some("h2"),
        "removing the fault must restore the h2 offer"
    );

    let _ = manager.delete_imposter(21552).await;
}

#[tokio::test]
async fn a_script_stub_imposter_does_not_advertise_h2() {
    // A `_rift.script` response forces HTTP/1-only for the same reason a TCP fault does, and is a
    // separate predicate in the same decision — so it needs its own pin.
    let manager = serve(serde_json::json!({
        "port": 21553, "protocol": "https",
        "stubs": [{"responses": [{"_rift": {"script": {
            "engine": "rhai",
            "code": "fn respond(ctx) { http(200, \"scripted\") }"
        }}}]}]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21553).await.as_deref(),
        Some("http/1.1"),
        "a scripted imposter is served HTTP/1-only, so it must not offer h2 either"
    );

    let _ = manager.delete_imposter(21553).await;
}

#[tokio::test]
async fn a_plaintext_imposter_with_a_fault_stub_is_unaffected() {
    // Regression guard for the branch this change does not touch: plaintext imposters have no
    // ALPN at all, and h2c prior-knowledge negotiation is unchanged.
    let manager = serve(serde_json::json!({
        "port": 21554, "protocol": "http",
        "stubs": [{"predicates": [{"equals": {"path": "/ok"}}],
                   "responses": [{"is": {"statusCode": 200, "body": "plain"}}]},
                  fault_stub()]
    }))
    .await;

    let body = reqwest::Client::new()
        .get("http://127.0.0.1:21554/ok")
        .send()
        .await
        .expect("plaintext request")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "plain", "the plaintext path must be untouched");

    let _ = manager.delete_imposter(21554).await;
}

#[tokio::test]
async fn a_negotiated_protocol_is_the_one_actually_served() {
    // Every other test here stops at the end of the handshake, so they prove what was OFFERED but
    // not what is then SPOKEN. That gap is precisely where this bug lived: the offer and the server
    // disagreeing. So drive a real request to completion in both states and assert the protocol
    // version of the response, not just the ALPN string.

    // HTTP/1-only state: a fault stub forces it, and a second stub answers a path so there is
    // something to serve without firing the fault.
    let h1 = serve(serde_json::json!({
        "port": 21555, "protocol": "https",
        "stubs": [{"predicates": [{"equals": {"path": "/ok"}}],
                   "responses": [{"is": {"statusCode": 200, "body": "h1-served"}}]},
                  fault_stub()]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21555).await.as_deref(),
        Some("http/1.1"),
        "precondition: this imposter is HTTP/1-only"
    );

    let resp = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client")
        .get("https://127.0.0.1:21555/ok")
        .send()
        .await
        .expect("request over the negotiated protocol");
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert_eq!(resp.text().await.expect("body"), "h1-served");
    let _ = h1.delete_imposter(21555).await;

    // HTTP/2 state: nothing forces HTTP/1, so the offer includes h2 and the server must speak it.
    let h2 = serve(serde_json::json!({
        "port": 21556, "protocol": "https",
        "stubs": [{"predicates": [{"equals": {"path": "/ok"}}],
                   "responses": [{"is": {"statusCode": 200, "body": "h2-served"}}]}]
    }))
    .await;

    assert_eq!(
        negotiated_alpn(21556).await.as_deref(),
        Some("h2"),
        "precondition: this imposter negotiates h2"
    );

    let resp = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client")
        .get("https://127.0.0.1:21556/ok")
        .send()
        .await
        .expect("request over the negotiated protocol");
    assert_eq!(
        resp.version(),
        reqwest::Version::HTTP_2,
        "the server advertised h2, so it must actually serve h2 — an advertisement the server \
         does not honour is the whole defect"
    );
    assert_eq!(resp.text().await.expect("body"), "h2-served");
    let _ = h2.delete_imposter(21556).await;
}

/// The imposter serves a self-signed certificate; this is a test of protocol negotiation, not of
/// trust, so the client accepts any certificate.
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

    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    cfg.dangerous().set_certificate_verifier(Arc::new(NoVerify));
    cfg
}
