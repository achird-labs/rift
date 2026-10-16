//! TLS clients that trust exactly one test CA (feature 20's imposters, feature 12's intercept
//! leaves), and what a completed handshake shows: the peer's chain and the protocol ALPN settled
//! on. A handshake that completes against a client trusting only that CA proves the peer served a
//! chain the CA signed, never a self-signed default.

use crate::wire::{Io, RawConnection, WIRE_TIMEOUT};
use anyhow::{Context as _, anyhow, bail};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use std::sync::Arc;

/// `h2` then `http/1.1`: what an HTTP/2-preferring client offers.
pub const ALPN_H2_FIRST: &[&str] = &["h2", "http/1.1"];
/// `http/1.1` only: a client that frames HTTP/1.1 by hand.
pub const ALPN_HTTP1: &[&str] = &["http/1.1"];

/// The certificates in a PEM document, in order.
pub fn certs_from_pem(pem: &[u8]) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing PEM certificates")?;
    if certs.is_empty() {
        bail!("no certificate in the PEM document");
    }
    Ok(certs)
}

/// A client configuration trusting only `roots`, offering `alpn`.
pub fn client_config(
    roots: &[CertificateDer<'static>],
    alpn: &[&str],
) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut store = rustls::RootCertStore::empty();
    for root in roots {
        store.add(root.clone()).context("adding a trust anchor")?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(store)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    Ok(Arc::new(config))
}

/// A TLS handshake over `io` for `server_name` (SNI and the name the chain is verified against).
/// The connection keeps the chain the peer presented and the protocol ALPN chose.
pub async fn handshake(
    io: Box<dyn Io>,
    server_name: &str,
    config: Arc<rustls::ClientConfig>,
) -> anyhow::Result<RawConnection> {
    let name = ServerName::try_from(server_name.to_string())
        .with_context(|| format!("TLS server name {server_name}"))?;
    let stream = tokio::time::timeout(
        WIRE_TIMEOUT,
        tokio_rustls::TlsConnector::from(config).connect(name, io),
    )
    .await
    .map_err(|_| anyhow!("the TLS handshake with {server_name} took over {WIRE_TIMEOUT:?}"))?
    .with_context(|| format!("the TLS handshake with {server_name}"))?;
    let (_, session) = stream.get_ref();
    let chain = session
        .peer_certificates()
        .map(|c| c.iter().map(|d| d.clone().into_owned()).collect())
        .unwrap_or_default();
    let alpn = session.alpn_protocol().map(<[u8]>::to_vec);
    Ok(RawConnection::new(Box::new(stream), chain, alpn))
}

/// A certificate's subject the way the JDK's `X500Principal.getName()` prints it (RFC 2253): the
/// most specific attribute first, comma-separated, e.g. `CN=rift-conformance-test-server-leaf`.
pub fn subject(der: &CertificateDer<'_>) -> anyhow::Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|e| anyhow!("parsing a peer certificate: {e}"))?;
    let registry = x509_parser::objects::oid_registry();
    let mut parts = Vec::new();
    let rdns: Vec<_> = cert.subject().iter().collect();
    for rdn in rdns.into_iter().rev() {
        for attr in rdn.iter() {
            let oid = attr.attr_type();
            let name = x509_parser::objects::oid2abbrev(oid, registry)
                .map_or_else(|_| oid.to_id_string(), str::to_string);
            let value = attr
                .as_str()
                .map_err(|e| anyhow!("subject attribute {name} is not a string: {e}"))?;
            parts.push(format!("{name}={value}"));
        }
    }
    Ok(parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> Vec<u8> {
        std::fs::read(crate::spec::fixtures_dir().join(path)).expect("a committed fixture")
    }

    #[test]
    fn the_fixture_leaf_subject_reads_as_the_jdk_prints_it() {
        let certs = certs_from_pem(&fixture("tls/server-leaf.pem")).expect("PEM");
        assert_eq!(
            subject(&certs[0]).expect("a subject"),
            "CN=rift-conformance-test-server-leaf"
        );
    }

    #[test]
    fn a_pem_without_a_certificate_is_an_error() {
        assert!(certs_from_pem(b"no cert").is_err());
        assert!(
            client_config(
                &certs_from_pem(&fixture("intercept/ca-cert.pem")).expect("CA"),
                ALPN_H2_FIRST
            )
            .is_ok()
        );
    }
}
