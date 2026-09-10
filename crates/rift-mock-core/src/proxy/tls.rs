//! TLS utilities for the proxy server.
//!
//! This module provides TLS-related functionality including certificate loading
//! and a no-op certificate verifier for development/testing.

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// No-op certificate verifier for development/testing with self-signed certificates.
///
/// # Warning
/// This disables all TLS security checks - use only in development!
#[derive(Debug)]
pub struct NoVerifier {
    /// Taken from the crypto provider rather than hand-listed (issue #974): a hard-coded list
    /// silently disagrees with the provider the connection actually negotiates with.
    schemes: Vec<rustls::SignatureScheme>,
}

impl NoVerifier {
    #[must_use]
    pub fn new(provider: &rustls::crypto::CryptoProvider) -> Self {
        Self {
            schemes: provider
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}

/// What an HTTPS imposter asks of its clients (issue #977).
///
/// Derived once, at imposter creation, from `mutualAuth` / `rejectUnauthorized` / `ca`. The
/// invalid combinations are rejected there, so anything reaching this layer is a decision rather
/// than a config to re-interpret.
///
/// [`Verify`](Self::Verify) carries PEM-*decoded* DER, which is not the same as *usable*:
/// `rustls_pemfile` only splits blocks, while `RootCertStore::add_parsable_certificates` runs
/// webpki's trust-anchor validation and can still reject a well-formed CERTIFICATE that is not a
/// valid anchor. That is why the `ignored > 0` guard below is reachable — it is not dead code.
#[derive(Debug, Clone)]
pub enum ClientAuth {
    /// No client certificate is requested. The default, and every imposter's behaviour before #977.
    Off,
    /// A client certificate is **required**, but its chain is not validated: any certificate whose
    /// private key the peer can prove it holds is accepted. This virtualizes a server that demands
    /// mutual auth without the test needing that server's real PKI.
    RequireAny,
    /// Required, and validated against these trust anchors.
    Verify { ca: Vec<CertificateDer<'static>> },
}

/// Accepts any client certificate chain, while still verifying the handshake signature.
///
/// The signature checks are deliberately **not** stubbed out the way [`NoVerifier`]'s are. That one
/// is for outbound development against a self-signed origin, where the whole point is to skip
/// verification. Here the point is the opposite: `mutualAuth` without `rejectUnauthorized` means
/// "I do not have your PKI", not "I will accept anything" — so "any certificate" must still mean
/// "any certificate you hold the key for", or the imposter would accept a chain copied off the wire.
#[derive(Debug)]
pub struct AcceptAnyClientCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl AcceptAnyClientCert {
    #[must_use]
    pub fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self { provider }
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // No hints: we accept any issuer, so naming some would mislead a client into filtering.
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Required, not merely requested: a SUT that forgot its client certificate should see the
        // handshake fail, which is what a real mTLS gateway does.
        true
    }
}

/// The ALPN list to advertise, as a pure function of the h2 kill switch (issue #996, #1029).
///
/// Shared by the imposter listener (here) and the intercept listener
/// (`rift_http_proxy::intercept`), which read the same flag for the same reason: advertising a
/// protocol the handshake did not offer — or offering one the server cannot serve — is the one
/// ALPN combination that breaks a client outright, so both call sites route through one function
/// rather than risk the list drifting apart.
#[must_use]
pub fn alpn_protocols(http1_only: bool) -> Vec<Vec<u8>> {
    if http1_only {
        vec![b"http/1.1".to_vec()]
    } else {
        // h2 first: ALPN is server-preference-ordered.
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    }
}

/// A pair of TLS acceptors for one imposter, differing only in what they advertise via ALPN
/// (issue #1029).
///
/// Built from a single [`rustls::ServerConfig`] cloned once *after* certs, client-auth and session
/// resumption are configured, so the two acceptors share one session cache and one ticketer
/// (`session_storage`/`ticketer` are `Arc`s under `Clone`) — a session resumed under one ALPN
/// offer stays valid under the other. Certs and key are parsed exactly once.
///
/// Two acceptors rather than one rebuilt per connection: whether an imposter is HTTP/1-only
/// depends on the *live* stub set, which can change between any two connections without the
/// acceptor being rebuilt (stub replace / hot reload). Picking one of two pre-built acceptors per
/// connection, with the same predicate that picks the hyper builder, is cheap and keeps the ALPN
/// offer and the actual protocol decision from ever disagreeing.
#[derive(Clone)]
pub struct TlsAcceptors {
    /// Offers h2 and http/1.1; used when the connection will be auto-negotiated.
    pub negotiated: TlsAcceptor,
    /// Offers http/1.1 only; used when the connection will be served HTTP/1-only.
    pub http1_only: TlsAcceptor,
}

impl TlsAcceptors {
    /// The acceptor whose ALPN offer matches how this connection will actually be served.
    ///
    /// The mapping lives here rather than at the call site because the two fields have the same
    /// type: transposing them would compile, and would reintroduce exactly the mismatch this issue
    /// fixed — advertising h2 to a client the server then answers in HTTP/1.
    #[must_use]
    pub fn for_connection(&self, http1_only: bool) -> &TlsAcceptor {
        if http1_only {
            &self.http1_only
        } else {
            &self.negotiated
        }
    }
}

/// Build a [`rustls::ServerConfig`] from in-memory PEM bytes (per-imposter HTTPS, issue #206).
///
/// Does not set `alpn_protocols` — callers decide that, per issue #1029 — but does configure
/// session resumption, since that is independent of ALPN and every caller wants it.
pub fn tls_server_config_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_auth: &ClientAuth,
) -> Result<rustls::ServerConfig, anyhow::Error> {
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate PEM: {e}"))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in certificate PEM");
    }

    // Accepts PKCS8, RSA, or EC private keys.
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|e| anyhow::anyhow!("Failed to parse private key PEM: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in key PEM"))?;

    // An explicit provider rather than the process default (issue #977): the client-cert verifiers
    // below need a provider handle anyway, and taking it here means this no longer depends on
    // `install_default()` having run — the same fix #974 made on the outbound side.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("Failed to build TLS configuration: {e}"))?;
    let builder = match client_auth {
        ClientAuth::Off => builder.with_no_client_auth(),
        ClientAuth::RequireAny => builder
            .with_client_cert_verifier(Arc::new(AcceptAnyClientCert::new(Arc::clone(&provider)))),
        ClientAuth::Verify { ca } => {
            let mut roots = rustls::RootCertStore::empty();
            let (added, ignored) = roots.add_parsable_certificates(ca.iter().cloned());
            // Fail closed: a caller asked for validation against specific anchors, so quietly
            // validating against fewer of them than they supplied is the wrong answer.
            if ignored > 0 {
                anyhow::bail!(
                    "{ignored} of the supplied client-auth CA certificate(s) are unusable by rustls"
                );
            }
            if added == 0 {
                anyhow::bail!("no usable client-auth CA certificates were supplied");
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::clone(&provider),
            )
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build the client-certificate verifier: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder.with_single_cert(certs, key).map_err(|e| {
        anyhow::anyhow!("Failed to build TLS configuration (cert/key mismatch?): {e}")
    })?;
    configure_session_resumption(&mut config)?;

    Ok(config)
}

/// Build the [`TlsAcceptors`] pair for one imposter from in-memory PEM bytes (issue #1029).
///
/// When [`crate::util::http2_disabled`] is set, both fields hold the http1-only acceptor: the kill
/// switch is process-wide and read once, so it can be folded in at construction rather than
/// re-checked per connection.
pub fn tls_acceptors_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_auth: &ClientAuth,
) -> Result<TlsAcceptors, anyhow::Error> {
    let config = tls_server_config_from_pem(cert_pem, key_pem, client_auth)?;
    Ok(tls_acceptors_from_config(
        config,
        crate::util::http2_disabled(),
    ))
}

/// Split one configured [`rustls::ServerConfig`] into the acceptor pair.
///
/// Takes `process_http1_only` as a parameter rather than reading
/// [`crate::util::http2_disabled`] itself, so the collapse branch is reachable from a test: that
/// function is a process-wide `OnceLock`, and a test that set the environment could neither
/// un-set it nor avoid racing every other test in the binary.
fn tls_acceptors_from_config(
    config: rustls::ServerConfig,
    process_http1_only: bool,
) -> TlsAcceptors {
    let mut http1_only_config = config.clone();
    http1_only_config.alpn_protocols = alpn_protocols(true);
    let http1_only = TlsAcceptor::from(Arc::new(http1_only_config));

    let negotiated = if process_http1_only {
        http1_only.clone()
    } else {
        let mut negotiated_config = config;
        negotiated_config.alpn_protocols = alpn_protocols(false);
        TlsAcceptor::from(Arc::new(negotiated_config))
    };

    TlsAcceptors {
        negotiated,
        http1_only,
    }
}

/// In-memory server session-cache capacity for TLS resumption (issue #705). Sized well above a
/// load generator's concurrent-reconnect working set so resumed handshakes are not evicted under
/// a handshake storm; each entry is small (a resumption secret + metadata).
pub const TLS_SESSION_CACHE_SIZE: usize = 8192;

/// Configure explicit TLS session resumption on a serve-side [`rustls::ServerConfig`] (issue #705).
///
/// Mock-server load is handshake-storm-shaped — load generators and test suites open many fresh
/// connections — and a resumed handshake skips the asymmetric crypto entirely, so resumption is the
/// dominant TLS lever here. rustls' server defaults leave a small session cache and issue no TLS 1.3
/// tickets unless configured, so both are set explicitly:
///
/// - a sized [`ServerSessionMemoryCache`](rustls::server::ServerSessionMemoryCache) for TLS 1.2
///   session IDs and TLS 1.3 stateful resumption, and
/// - a `ring`-backed [`Ticketer`](rustls::crypto::ring::Ticketer) for stateless session tickets
///   (TLS 1.3 tickets and TLS 1.2 RFC 5077). It auto-rotates its ticket-encryption key (~6h), so old
///   tickets stay decryptable across a rotation while the signing key moves forward.
///
/// Crypto provider: `ring` is pinned deliberately (see `Cargo.toml`) — `aws-lc-rs` fails to build on
/// the windows-msvc CI runner and would break the FFI cross-compile matrix, so it is not a viable
/// alternative regardless of its bulk-throughput edge; for the small responses a mock serves the
/// handshake rate dominates, and `ring` is competitive there.
pub fn configure_session_resumption(
    config: &mut rustls::ServerConfig,
) -> Result<(), anyhow::Error> {
    config.session_storage = rustls::server::ServerSessionMemoryCache::new(TLS_SESSION_CACHE_SIZE);
    config.ticketer = rustls::crypto::ring::Ticketer::new()
        .map_err(|e| anyhow::anyhow!("Failed to build TLS session ticketer: {e}"))?;
    Ok(())
}

/// Generate an in-memory self-signed [`TlsAcceptors`] pair for zero-config HTTPS imposters (issue
/// #206), matching Mountebank's built-in self-signed default. Valid for `localhost`/`127.0.0.1`.
pub fn generate_self_signed_acceptors(
    client_auth: &ClientAuth,
) -> Result<TlsAcceptors, anyhow::Error> {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| anyhow::anyhow!("Failed to generate self-signed certificate: {e}"))?;
    tls_acceptors_from_pem(
        cert.cert.pem().as_bytes(),
        cert.key_pair.serialize_pem().as_bytes(),
        client_auth,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_verifier_supported_schemes() {
        let verifier = NoVerifier::new(&rustls::crypto::ring::default_provider());
        let schemes = verifier.supported_verify_schemes();
        assert!(!schemes.is_empty());
        assert!(schemes.contains(&rustls::SignatureScheme::RSA_PKCS1_SHA256));
        assert!(schemes.contains(&rustls::SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(schemes.contains(&rustls::SignatureScheme::ED25519));
        assert!(schemes.contains(&rustls::SignatureScheme::RSA_PSS_SHA256));
    }

    fn test_server_config() -> rustls::ServerConfig {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let certs = rustls_pemfile::certs(&mut cert.cert.pem().as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(&mut cert.key_pair.serialize_pem().as_bytes())
            .unwrap()
            .unwrap();
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap()
    }

    #[test]
    fn session_resumption_installs_ticketer_and_survives_default() {
        // Issue #705: rustls' default server config issues no TLS 1.3 tickets — resumption must be
        // configured explicitly. Prove the default is off, then that the helper turns it on.
        let mut config = test_server_config();
        assert!(
            !config.ticketer.enabled(),
            "the rustls default must not issue tickets (else this test proves nothing)"
        );
        assert!(
            config.send_tls13_tickets > 0,
            "rustls still asks for N>0 tickets by default"
        );

        configure_session_resumption(&mut config).expect("ticketer builds under the ring provider");
        assert!(
            config.ticketer.enabled(),
            "after configuration the TLS 1.3 ticketer must be enabled for stateless resumption"
        );
    }

    #[test]
    fn https_acceptor_builds_with_resumption() {
        // The real imposter-HTTPS path (self-signed) must still build once resumption is wired in.
        assert!(generate_self_signed_acceptors(&ClientAuth::Off).is_ok());
    }

    /// A configured `ServerConfig` off the real self-signed path, so these tests exercise the same
    /// construction an imposter does rather than a hand-rolled config.
    fn self_signed_config() -> rustls::ServerConfig {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("self-signed cert");
        tls_server_config_from_pem(
            cert.cert.pem().as_bytes(),
            cert.key_pair.serialize_pem().as_bytes(),
            &ClientAuth::Off,
        )
        .expect("server config")
    }

    #[test]
    fn the_two_acceptors_share_one_session_cache_and_ticketer() {
        // Issue #1029: the pair exists so the ALPN offer can follow a stub mutation. If each half
        // carried its own session cache, a mutation would silently move a client to the other
        // acceptor and its session would no longer resume — turning a correctness fix into a
        // handshake-storm regression (#705 is why resumption matters here at all). Cloning the
        // config AFTER `configure_session_resumption` is what keeps the `Arc`s shared, so this
        // pins the ordering, not just the outcome.
        let acceptors = tls_acceptors_from_config(self_signed_config(), false);
        let negotiated = acceptors.negotiated.config();
        let http1_only = acceptors.http1_only.config();

        assert!(
            Arc::ptr_eq(&negotiated.session_storage, &http1_only.session_storage),
            "both acceptors must share ONE session cache, or resumption breaks across a stub change"
        );
        assert!(
            Arc::ptr_eq(&negotiated.ticketer, &http1_only.ticketer),
            "and one ticketer, for the same reason"
        );
    }

    #[test]
    fn the_pair_differs_only_in_its_alpn_offer() {
        let acceptors = tls_acceptors_from_config(self_signed_config(), false);
        assert_eq!(
            acceptors.negotiated.config().alpn_protocols,
            alpn_protocols(false),
            "the negotiated half offers h2 first"
        );
        assert_eq!(
            acceptors.http1_only.config().alpn_protocols,
            alpn_protocols(true),
            "the http1-only half offers http/1.1 alone"
        );
    }

    #[test]
    fn the_kill_switch_collapses_both_halves_to_http1_only() {
        // The `RIFT_DISABLE_HTTP2` branch. It is folded in at construction because the switch is a
        // process-wide `OnceLock` — constant for the life of the process, unlike the per-connection
        // fault/script half. Reachable here only because the fold takes a parameter rather than
        // reading the env itself: a test cannot un-set a `OnceLock`.
        let acceptors = tls_acceptors_from_config(self_signed_config(), true);

        assert_eq!(
            acceptors.negotiated.config().alpn_protocols,
            alpn_protocols(true),
            "with the kill switch on, even the `negotiated` half must not offer h2 — otherwise the \
             switch removes h2 from what is SERVED while still advertising it, which is the exact \
             mismatch this issue is about"
        );
        assert_eq!(
            acceptors.http1_only.config().alpn_protocols,
            alpn_protocols(true)
        );
    }

    #[test]
    fn alpn_offers_h2_unless_http1_only() {
        assert_eq!(
            alpn_protocols(false),
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "h2 must be offered first, matching the intercept listener's order (#996)"
        );
        assert_eq!(
            alpn_protocols(true),
            vec![b"http/1.1".to_vec()],
            "an HTTP/1-only decision must stop h2 being ADVERTISED, not merely stop it being \
             served: a client that negotiated h2 from ALPN and then met an HTTP/1-only server \
             would break outright, which is worse than the downgrade this exists to force"
        );
    }
}
