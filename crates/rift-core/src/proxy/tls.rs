//! TLS utilities for the proxy server.
//!
//! This module provides TLS-related functionality including certificate loading
//! and a no-op certificate verifier for development/testing.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
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
    // Load certificate chain
    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| anyhow::anyhow!("Failed to open certificate file '{cert_path}': {e}"))?;
    let mut cert_reader = std::io::BufReader::new(cert_file);
    let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate file: {e}"))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in certificate file: {cert_path}");
    }

    // Load private key
    let key_file = std::fs::File::open(key_path)
        .map_err(|e| anyhow::anyhow!("Failed to open private key file '{key_path}': {e}"))?;
    let mut key_reader = std::io::BufReader::new(key_file);

    // Try reading as PKCS8, RSA, or EC private key
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| anyhow::anyhow!("Failed to parse private key file: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in key file: {key_path}"))?;

    // Build TLS server configuration
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("Failed to build TLS configuration: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
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
