//! Intercept CA and per-SNI leaf certificate minting for TLS-MITM interception
//! (epic #394, slice 1/5).
//!
//! A [`CertificateAuthority`] is generated once (or loaded from PEM) and mints per-host leaf
//! certificates signed by the CA on demand. [`SniCertResolver`] adapts that into a rustls
//! [`ResolvesServerCert`] so an interception listener can terminate TLS for any SNI without
//! pre-provisioning certificates. The types are public because the forward-proxy listener
//! (slice 3) and the truststore/admin surface (slices 2 & 4) live in the sibling
//! `rift-http-proxy` crate.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// An in-memory certificate authority that mints per-host leaf certificates on demand.
pub struct CertificateAuthority {
    /// The issuing certificate. Its `params` (distinguished name, key-id method) drive the
    /// issuer identity written into every minted leaf; when the CA is loaded from PEM this is
    /// re-derived from the input so leaves chain to the original certificate.
    issuer: Certificate,
    key: KeyPair,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
}

impl std::fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the key material; the CA is identified by its certificate PEM.
        f.debug_struct("CertificateAuthority")
            .field("cert_pem", &self.cert_pem)
            .finish_non_exhaustive()
    }
}

impl CertificateAuthority {
    /// Generate a fresh in-memory CA.
    pub fn generate() -> anyhow::Result<Self> {
        let key = KeyPair::generate().map_err(|e| anyhow::anyhow!("generate CA key: {e}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| anyhow::anyhow!("build CA params: {e}"))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params
            .distinguished_name
            .push(DnType::CommonName, "Rift Intercept CA");
        let cert = params
            .self_signed(&key)
            .map_err(|e| anyhow::anyhow!("self-sign CA: {e}"))?;
        let cert_pem = cert.pem();
        let cert_der = cert.der().clone();
        Ok(Self {
            issuer: cert,
            key,
            cert_pem,
            cert_der,
        })
    }

    /// Load an existing CA from its certificate and private-key PEM. Leaves minted afterwards
    /// chain to the supplied certificate (its distinguished name is recovered so the issuer
    /// identity matches).
    pub fn load_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        let key =
            KeyPair::from_pem(key_pem).map_err(|e| anyhow::anyhow!("parse CA key PEM: {e}"))?;
        let params = CertificateParams::from_ca_cert_pem(cert_pem)
            .map_err(|e| anyhow::anyhow!("parse CA cert PEM: {e}"))?;
        // Re-derive an issuing certificate from the parsed params so `signed_by` writes the
        // original CA's distinguished name into leaves. The original PEM/DER remain the trust
        // anchor consumers pin.
        let issuer = params
            .self_signed(&key)
            .map_err(|e| anyhow::anyhow!("rebuild issuer from CA PEM: {e}"))?;
        let cert_der = pem_to_der(cert_pem)?;
        Ok(Self {
            issuer,
            key,
            cert_pem: cert_pem.to_string(),
            cert_der,
        })
    }

    /// Load the CA from PEM files when both paths are supplied, generate a fresh one when neither
    /// is, and reject a half-configured pair (both-or-neither). This is the single implementation
    /// shared by the container adapter (`--intercept-ca-cert`/`--intercept-ca-key`) and the
    /// embedded FFI (`caCertPath`/`caKeyPath`), so both surfaces agree on load-or-generate
    /// semantics and error.
    pub fn load_or_generate(
        cert_path: Option<&Path>,
        key_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        match (cert_path, key_path) {
            (Some(cert), Some(key)) => {
                let cert_pem = std::fs::read_to_string(cert)
                    .with_context(|| format!("reading intercept CA cert {}", cert.display()))?;
                let key_pem = std::fs::read_to_string(key)
                    .with_context(|| format!("reading intercept CA key {}", key.display()))?;
                Self::load_pem(&cert_pem, &key_pem)
            }
            (None, None) => Self::generate(),
            _ => anyhow::bail!(
                "intercept CA cert and key must be provided together (or both omitted to generate a CA)"
            ),
        }
    }

    /// The CA certificate as PEM.
    pub fn ca_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The CA certificate in DER form (the trust anchor).
    pub fn ca_cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// Mint a leaf certificate valid for `host`, signed by this CA, packaged with its private
    /// key and the full chain (`[leaf, ca]`) as a rustls [`CertifiedKey`].
    pub fn mint_leaf(&self, host: &str) -> anyhow::Result<CertifiedKey> {
        self.mint_leaf_with_provider(host, &default_provider())
    }

    fn mint_leaf_with_provider(
        &self,
        host: &str,
        provider: &CryptoProvider,
    ) -> anyhow::Result<CertifiedKey> {
        let leaf_key =
            KeyPair::generate().map_err(|e| anyhow::anyhow!("generate leaf key: {e}"))?;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .map_err(|e| anyhow::anyhow!("build leaf params for {host}: {e}"))?;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params
            .signed_by(&leaf_key, &self.issuer, &self.key)
            .map_err(|e| anyhow::anyhow!("sign leaf for {host}: {e}"))?;

        let chain = vec![leaf.der().clone(), self.cert_der.clone()];
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        CertifiedKey::from_der(chain, key_der, provider)
            .map_err(|e| anyhow::anyhow!("assemble certified key for {host}: {e}"))
    }
}

fn pem_to_der(cert_pem: &str) -> anyhow::Result<CertificateDer<'static>> {
    let mut reader = cert_pem.as_bytes();
    let mut certs = rustls_pemfile::certs(&mut reader);
    let first = certs
        .next()
        .ok_or_else(|| anyhow::anyhow!("no certificate found in CA PEM"))?
        .map_err(|e| anyhow::anyhow!("parse CA cert PEM: {e}"))?;
    if certs.next().is_some() {
        tracing::warn!(
            "CA PEM contains multiple certificates; pinning the first as the trust anchor"
        );
    }
    Ok(first)
}

/// A rustls certificate resolver that mints (and caches) one leaf per SNI host via the CA.
#[derive(Debug)]
pub struct SniCertResolver {
    ca: Arc<CertificateAuthority>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl SniCertResolver {
    pub fn new(ca: Arc<CertificateAuthority>) -> Self {
        Self {
            ca,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Return the certified key for `host`, minting and caching it on first request.
    pub fn cert_for(&self, host: &str) -> anyhow::Result<Arc<CertifiedKey>> {
        if let Some(ck) = self.lock_cache().get(host) {
            return Ok(ck.clone());
        }
        let minted = Arc::new(self.ca.mint_leaf(host)?);
        // Another thread may have minted concurrently; `or_insert` keeps the first-inserted key
        // and every caller converges on it — the extra leaf is simply dropped.
        Ok(self
            .lock_cache()
            .entry(host.to_string())
            .or_insert(minted)
            .clone())
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CertifiedKey>>> {
        // A poisoned lock only means a thread panicked while holding it; the cached certificates
        // remain valid, so recover rather than permanently break interception for the process.
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let Some(host) = client_hello.server_name() else {
            tracing::debug!("intercept TLS: client sent no SNI; cannot select a leaf certificate");
            return None;
        };
        match self.cert_for(host) {
            Ok(ck) => Some(ck),
            Err(e) => {
                // resolve() must return Option; log before dropping so a failed MITM handshake
                // is diagnosable instead of a silent generic TLS abort.
                tracing::warn!(host, error = %e, "intercept TLS: failed to mint leaf certificate");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::RootCertStore;
    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    /// Verify `leaf` (with `intermediates`) is signed by `ca` and valid for `host` — a genuine
    /// signature-chain + SAN check via rustls/webpki, trusting only the CA.
    fn assert_chains_to_ca(
        ca: &CertificateAuthority,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        host: &str,
    ) {
        let mut roots = RootCertStore::empty();
        roots.add(ca.ca_cert_der().clone()).expect("add CA root");
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(default_provider()),
        )
        .build()
        .expect("build verifier");
        let name = ServerName::try_from(host.to_string()).expect("server name");
        verifier
            .verify_server_cert(leaf, intermediates, &name, &[], UnixTime::now())
            .unwrap_or_else(|e| panic!("leaf for {host} should chain to CA: {e:?}"));
    }

    #[test]
    fn generate_produces_usable_ca() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        assert!(ca.ca_cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        // A usable CA can mint a leaf.
        ca.mint_leaf("example.com").expect("mint leaf");
    }

    #[test]
    fn minted_leaf_has_san_and_chains_to_ca() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let ck = ca.mint_leaf("cdn.example.com").expect("mint leaf");
        // chain is [leaf, ca]
        assert_eq!(ck.cert.len(), 2, "chain should be leaf + CA");
        assert_eq!(&ck.cert[1], ca.ca_cert_der(), "second entry is the CA cert");
        // Genuine signature + SAN verification against the CA as the only trust anchor.
        assert_chains_to_ca(&ca, &ck.cert[0], &[], "cdn.example.com");
        // Wrong host must NOT verify against the same leaf.
        let mut roots = RootCertStore::empty();
        roots.add(ca.ca_cert_der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(default_provider()),
        )
        .build()
        .unwrap();
        let wrong = ServerName::try_from("other.example.org").unwrap();
        assert!(
            verifier
                .verify_server_cert(&ck.cert[0], &[], &wrong, &[], UnixTime::now())
                .is_err(),
            "leaf minted for cdn.example.com must not be valid for other.example.org"
        );
    }

    #[test]
    fn resolver_caches_by_host() {
        let ca = Arc::new(CertificateAuthority::generate().expect("generate CA"));
        let resolver = SniCertResolver::new(ca);
        let a1 = resolver.cert_for("a.example.com").expect("mint a");
        let a2 = resolver.cert_for("a.example.com").expect("cache hit a");
        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same host must return the cached key"
        );
        let b = resolver.cert_for("b.example.com").expect("mint b");
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "different host must mint a distinct key"
        );
    }

    #[test]
    fn load_pem_mints_leaves_chaining_to_loaded_ca() {
        // Build a CA out-of-band, serialise it, and load it back — the parity property.
        let original = CertificateAuthority::generate().expect("generate CA");
        // Reconstruct PEMs a persisted CA would carry: cert PEM + key PEM.
        let cert_pem = original.ca_cert_pem().to_string();
        let key_pem = original.key.serialize_pem();

        let loaded = CertificateAuthority::load_pem(&cert_pem, &key_pem).expect("load CA");
        assert_eq!(
            loaded.ca_cert_pem(),
            cert_pem,
            "loaded CA exposes the original certificate PEM"
        );
        let ck = loaded
            .mint_leaf("svc.internal")
            .expect("mint via loaded CA");
        assert_chains_to_ca(&loaded, &ck.cert[0], &[], "svc.internal");
    }

    #[test]
    fn load_pem_rejects_garbage() {
        assert!(
            CertificateAuthority::load_pem("not a pem", "also not a pem").is_err(),
            "malformed PEM input must be a typed error, not a panic or silent success"
        );
    }

    #[test]
    fn cert_for_dedups_under_concurrent_load() {
        use std::thread;
        let resolver = Arc::new(SniCertResolver::new(Arc::new(
            CertificateAuthority::generate().expect("generate CA"),
        )));
        let keys: Vec<_> = (0..8)
            .map(|_| {
                let r = resolver.clone();
                thread::spawn(move || r.cert_for("race.example.com").expect("mint under race"))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().expect("thread panicked"))
            .collect();
        let canonical = resolver.cert_for("race.example.com").expect("cache hit");
        assert!(
            keys.iter().all(|k| Arc::ptr_eq(k, &canonical)),
            "all concurrent callers must converge on the single cached key"
        );
    }
}
