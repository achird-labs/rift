//! TLS utilities for the proxy server.
//!
//! This module provides TLS-related functionality including certificate loading
//! and a no-op certificate verifier for development/testing.

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// No-op certificate verifier for development/testing with self-signed certificates.
///
/// # Warning
/// This disables all TLS security checks - use only in development!
#[derive(Debug)]
pub struct NoVerifier;

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
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

/// Create TLS acceptor from certificate and key files.
pub fn create_tls_acceptor(cert_path: &str, key_path: &str) -> Result<TlsAcceptor, anyhow::Error> {
    let cert_pem = std::fs::read(cert_path)
        .map_err(|e| anyhow::anyhow!("Failed to open certificate file '{cert_path}': {e}"))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| anyhow::anyhow!("Failed to open private key file '{key_path}': {e}"))?;
    tls_acceptor_from_pem(&cert_pem, &key_pem)
}

/// Create a TLS acceptor from in-memory PEM bytes (per-imposter HTTPS, issue #206).
pub fn tls_acceptor_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<TlsAcceptor, anyhow::Error> {
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

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            anyhow::anyhow!("Failed to build TLS configuration (cert/key mismatch?): {e}")
        })?;
    // Advertise HTTP/2 and HTTP/1.1 via ALPN so TLS clients can negotiate h2 (issue #295).
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Generate an in-memory self-signed acceptor for zero-config HTTPS imposters (issue #206),
/// matching Mountebank's built-in self-signed default. Valid for `localhost`/`127.0.0.1`.
pub fn generate_self_signed_acceptor() -> Result<TlsAcceptor, anyhow::Error> {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| anyhow::anyhow!("Failed to generate self-signed certificate: {e}"))?;
    tls_acceptor_from_pem(
        cert.cert.pem().as_bytes(),
        cert.key_pair.serialize_pem().as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_verifier_supported_schemes() {
        let verifier = NoVerifier;
        let schemes = verifier.supported_verify_schemes();
        assert!(!schemes.is_empty());
        assert!(schemes.contains(&rustls::SignatureScheme::RSA_PKCS1_SHA256));
        assert!(schemes.contains(&rustls::SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(schemes.contains(&rustls::SignatureScheme::ED25519));
        assert!(schemes.contains(&rustls::SignatureScheme::RSA_PSS_SHA256));
    }
}
