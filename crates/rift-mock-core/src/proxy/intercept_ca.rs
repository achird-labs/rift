//! Intercept CA and per-SNI leaf certificate minting for TLS-MITM interception
//! (epic #394, slice 1/5).
//!
//! A [`CertificateAuthority`] is generated once (or loaded from PEM) and mints per-host leaf
//! certificates signed by the CA on demand. [`SniCertResolver`] adapts that into a rustls
//! [`ResolvesServerCert`] so an interception listener can terminate TLS for any SNI without
//! pre-provisioning certificates. The types are public because the forward-proxy listener
//! (slice 3) and the truststore/admin surface (slices 2 & 4) live in the sibling
//! `rift-http-proxy` crate.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lru::LruCache;

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

/// Where a [`CertificateAuthority`] comes from, resolved once from the three mutually-exclusive
/// input pairs an intercept start accepts (issue #593). Building it via [`CaSource::resolve`] keeps
/// the "both-or-neither per pair, at most one pair" validation in a single place shared by the
/// admin API, the FFI, and the CLI/env launch path.
pub enum CaSource {
    /// Mint a fresh in-memory CA (the default when no CA input is supplied).
    Generate,
    /// Load the CA from PEM files on the engine's filesystem.
    Paths { cert: PathBuf, key: PathBuf },
    /// Load the CA from inline PEM bytes carried in the request/env.
    Pem { cert: String, key: String },
}

impl std::fmt::Debug for CaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render inline PEM — the `Pem` key is secret material. Paths are safe to show.
        match self {
            CaSource::Generate => f.write_str("Generate"),
            CaSource::Paths { cert, key } => f
                .debug_struct("Paths")
                .field("cert", cert)
                .field("key", key)
                .finish(),
            CaSource::Pem { .. } => f.write_str("Pem { .. }"),
        }
    }
}

/// Every way a start request can name its CA, before they are reduced to one [`CaSource`]: a file
/// pair, an inline PEM pair, or a pair of environment variable *names* whose values are the PEMs
/// (issue #1293 — a secret store such as ECS's delivers secrets only as environment variables).
#[derive(Default, Clone)]
pub struct CaInputs {
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    pub cert_pem: Option<String>,
    pub key_pem: Option<String>,
    pub cert_pem_env: Option<String>,
    pub key_pem_env: Option<String>,
}

impl std::fmt::Debug for CaInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Inline PEM is never rendered (the key is secret material); paths and env var *names* are.
        f.debug_struct("CaInputs")
            .field("cert_path", &self.cert_path)
            .field("key_path", &self.key_path)
            .field("cert_pem", &self.cert_pem.as_ref().map(|_| "<pem>"))
            .field("key_pem", &self.key_pem.as_ref().map(|_| "<redacted>"))
            .field("cert_pem_env", &self.cert_pem_env)
            .field("key_pem_env", &self.key_pem_env)
            .finish()
    }
}

impl CaSource {
    /// Reduce a file pair and an inline PEM pair to a single source. See
    /// [`from_inputs`](Self::from_inputs), which also takes an env-named pair.
    pub fn resolve(
        cert_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        cert_pem: Option<String>,
        key_pem: Option<String>,
    ) -> anyhow::Result<Self> {
        Self::from_inputs(CaInputs {
            cert_path,
            key_path,
            cert_pem,
            key_pem,
            ..CaInputs::default()
        })
    }

    /// Reduce the three optional pairs to a single source, rejecting a half-supplied pair or more
    /// than one pair; none at all → [`CaSource::Generate`]. An env-named pair is read from the
    /// process environment here and becomes [`CaSource::Pem`]. Error messages never include PEM
    /// contents.
    pub fn from_inputs(inputs: CaInputs) -> anyhow::Result<Self> {
        Self::from_inputs_with_env(inputs, &|name| std::env::var(name))
    }

    /// [`from_inputs`](Self::from_inputs) with the environment lookup supplied, so the rules can be
    /// tested without writing to the process environment.
    pub(crate) fn from_inputs_with_env(
        inputs: CaInputs,
        env: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Self> {
        let CaInputs {
            cert_path,
            key_path,
            cert_pem,
            key_pem,
            cert_pem_env,
            key_pem_env,
        } = inputs;
        let paths = match (cert_path, key_path) {
            (Some(cert), Some(key)) => Some((cert, key)),
            (None, None) => None,
            _ => anyhow::bail!(
                "intercept CA cert and key paths must be provided together (or both omitted)"
            ),
        };
        let pem = match (cert_pem, key_pem) {
            (Some(cert), Some(key)) => Some((cert, key)),
            (None, None) => None,
            _ => anyhow::bail!(
                "intercept CA cert and key PEM must be provided together (or both omitted)"
            ),
        };
        let env_names = match (cert_pem_env, key_pem_env) {
            (Some(cert), Some(key)) => Some((cert, key)),
            (None, None) => None,
            _ => anyhow::bail!(
                "intercept CA cert and key env var names must be provided together (or both omitted)"
            ),
        };
        match (paths, pem, env_names) {
            (Some((cert, key)), None, None) => Ok(CaSource::Paths { cert, key }),
            (None, Some((cert, key)), None) => Ok(CaSource::Pem { cert, key }),
            (None, None, Some((cert, key))) => Ok(CaSource::Pem {
                cert: read_named_pem(env, &cert, "caCertPemEnv")?,
                key: read_named_pem(env, &key, "caKeyPemEnv")?,
            }),
            (None, None, None) => Ok(CaSource::Generate),
            (Some(_), Some(_), None) => anyhow::bail!(
                "intercept CA path and inline PEM are mutually exclusive — supply only one"
            ),
            _ => anyhow::bail!(
                "intercept CA path, inline PEM and env-named PEM are mutually exclusive — supply \
                 only one"
            ),
        }
    }

    /// True when this source mints a fresh CA (nothing was supplied). `returnCaKey` is only valid
    /// against a generated CA, so callers gate the key-return on this (issue #593, D4).
    pub fn is_generate(&self) -> bool {
        matches!(self, CaSource::Generate)
    }
}

/// The value of the environment variable `name`, which the `key` option named. An unset or
/// non-UTF-8 variable is an error naming both, never an empty CA and never a fallback.
fn read_named_pem(
    env: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    name: &str,
    key: &str,
) -> anyhow::Result<String> {
    env(name).map_err(|e| match e {
        std::env::VarError::NotPresent => {
            anyhow::anyhow!("environment variable {name} (named by {key}) is not set")
        }
        std::env::VarError::NotUnicode(_) => {
            anyhow::anyhow!("environment variable {name} (named by {key}) is not valid UTF-8")
        }
    })
}

/// How [`CertificateAuthority::generate_with`] shapes a CA an operator means to keep (issue
/// #1274): its subject name and how long it stays valid from now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateOptions {
    pub common_name: String,
    pub validity: std::time::Duration,
}

/// Backdating applied to a kept CA's `notBefore`, so a SUT whose clock runs a little behind the
/// machine that generated it does not see a not-yet-valid anchor.
const CLOCK_SKEW_ALLOWANCE: time::Duration = time::Duration::minutes(5);

impl CertificateAuthority {
    /// Generate a fresh in-memory CA. Keeps rcgen's default validity (1975 to 4096), which suits
    /// an anchor that lives as long as one listener and never meets a SUT with a skewed clock.
    pub fn generate() -> anyhow::Result<Self> {
        Self::generate_params("Rift Intercept CA", None)
    }

    /// Generate a CA meant to be written out and kept (issue #1274): named `common_name`, valid
    /// from five minutes ago until `validity` from now.
    pub fn generate_with(options: &GenerateOptions) -> anyhow::Result<Self> {
        let now = time::OffsetDateTime::now_utc();
        let validity = time::Duration::try_from(options.validity)
            .map_err(|e| anyhow::anyhow!("CA validity out of range: {e}"))?;
        let not_after = now
            .checked_add(validity)
            .ok_or_else(|| anyhow::anyhow!("CA validity out of range"))?;
        Self::generate_params(
            &options.common_name,
            Some((now - CLOCK_SKEW_ALLOWANCE, not_after)),
        )
    }

    fn generate_params(
        common_name: &str,
        validity: Option<(time::OffsetDateTime, time::OffsetDateTime)>,
    ) -> anyhow::Result<Self> {
        let key = KeyPair::generate().map_err(|e| anyhow::anyhow!("generate CA key: {e}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| anyhow::anyhow!("build CA params: {e}"))?;
        if let Some((not_before, not_after)) = validity {
            params.not_before = not_before;
            params.not_after = not_after;
        }
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
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

    /// Load or generate the CA per a resolved [`CaSource`] — the single implementation shared by
    /// the admin `POST /intercept`, the FFI `rift_start_intercept`, and the CLI/env launch path
    /// (issue #593), so every surface agrees on load-or-generate semantics and error. Errors never
    /// include PEM contents.
    pub fn from_source(source: &CaSource) -> anyhow::Result<Self> {
        match source {
            CaSource::Generate => Self::generate(),
            CaSource::Paths { cert, key } => {
                let cert_pem = std::fs::read_to_string(cert)
                    .with_context(|| format!("reading intercept CA cert {}", cert.display()))?;
                let key_pem = std::fs::read_to_string(key)
                    .with_context(|| format!("reading intercept CA key {}", key.display()))?;
                Self::load_pem(&cert_pem, &key_pem)
            }
            CaSource::Pem { cert, key } => Self::load_pem(cert, key),
        }
    }

    /// Load the CA from PEM files when both paths are supplied, generate a fresh one when neither
    /// is, and reject a half-configured pair (both-or-neither). Thin compat shim over
    /// [`CaSource::resolve`] + [`from_source`](Self::from_source) for existing path-only callers.
    pub fn load_or_generate(
        cert_path: Option<&Path>,
        key_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let source = CaSource::resolve(
            cert_path.map(Path::to_path_buf),
            key_path.map(Path::to_path_buf),
            None,
            None,
        )?;
        Self::from_source(&source)
    }

    /// The CA certificate as PEM.
    pub fn ca_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The CA private key as PKCS#8 PEM. Secret material — only returned when a caller explicitly
    /// asks to bootstrap a fresh CA (issue #593); never logged and never exposed by `GET /intercept`.
    pub fn ca_key_pem(&self) -> String {
        self.key.serialize_pem()
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
        // RFC 5280 4.2.1.1 requires an AKI on a CA-issued certificate, and OpenSSL's strict mode
        // (Python's `ssl` default since 3.13) rejects a leaf without one; webpki does not check
        // it, so only a parser-level test notices (issue #1277). rcgen derives the AKI from the
        // issuer's key identifier method, which `load_pem` sets to the loaded CA's own SKI when the
        // CA carries one (a CA without an SKI fails strict verification on its own anyway).
        params.use_authority_key_identifier_extension = true;
        params.is_ca = IsCa::ExplicitNoCa;
        params.distinguished_name.push(DnType::CommonName, host);
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

/// Hard cap on distinct SNI leaves held at once. The SNI is attacker-controlled on the intercept
/// listener, so an unbounded cache is a memory-exhaustion vector (issue #539); LRU eviction keeps
/// the working set of real upstream hosts hot while bounding the worst case. Generous relative to
/// any realistic upstream count.
const MAX_CACHED_LEAVES: usize = 1024;

/// A rustls certificate resolver that mints (and caches) one leaf per SNI host via the CA.
#[derive(Debug)]
pub struct SniCertResolver {
    ca: Arc<CertificateAuthority>,
    cache: Mutex<LruCache<String, Arc<CertifiedKey>>>,
}

impl SniCertResolver {
    pub fn new(ca: Arc<CertificateAuthority>) -> Self {
        let capacity = NonZeroUsize::new(MAX_CACHED_LEAVES).unwrap_or(NonZeroUsize::MIN);
        Self::with_capacity(ca, capacity)
    }

    fn with_capacity(ca: Arc<CertificateAuthority>, capacity: NonZeroUsize) -> Self {
        Self {
            ca,
            cache: Mutex::new(LruCache::new(capacity)),
        }
    }

    /// Return the certified key for `host`, minting and caching it on first request. Minting runs
    /// outside the cache lock so a slow keygen never serializes unrelated handshakes.
    pub fn cert_for(&self, host: &str) -> anyhow::Result<Arc<CertifiedKey>> {
        if let Some(ck) = self.lock_cache().get(host) {
            return Ok(ck.clone());
        }
        let minted = Arc::new(self.ca.mint_leaf(host)?);
        let mut cache = self.lock_cache();
        // Another thread may have minted concurrently; keep the first-inserted key so every caller
        // converges on it and the extra leaf is simply dropped.
        if let Some(existing) = cache.get(host) {
            return Ok(existing.clone());
        }
        cache.put(host.to_string(), minted.clone());
        Ok(minted)
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, LruCache<String, Arc<CertifiedKey>>> {
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
                tracing::warn!(host, error = %format_args!("{e:#}"), "intercept TLS: failed to mint leaf certificate");
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
    fn many_unique_snis_stay_bounded() {
        // Attacker floods unique SNIs; the cache must never exceed its hard cap (issue #539).
        let ca = Arc::new(CertificateAuthority::generate().expect("generate CA"));
        let resolver =
            SniCertResolver::with_capacity(ca, NonZeroUsize::new(8).expect("nonzero cap"));
        for i in 0..100 {
            resolver
                .cert_for(&format!("h{i}.evil.example"))
                .expect("mint leaf");
        }
        assert_eq!(
            resolver.lock_cache().len(),
            8,
            "cache must be bounded by its capacity regardless of distinct-SNI count"
        );
    }

    #[test]
    fn cache_evicts_lru_keeps_recently_used() {
        let ca = Arc::new(CertificateAuthority::generate().expect("generate CA"));
        let resolver =
            SniCertResolver::with_capacity(ca, NonZeroUsize::new(2).expect("nonzero cap"));

        let a1 = resolver.cert_for("a.example").expect("mint a");
        let b1 = resolver.cert_for("b.example").expect("mint b");
        // Touch "a" so "b" becomes the least-recently-used entry.
        let a2 = resolver.cert_for("a.example").expect("hit a");
        assert!(Arc::ptr_eq(&a1, &a2), "a stays cached and is bumped to MRU");

        // A third distinct host evicts the LRU entry ("b"), not "a".
        let _c = resolver.cert_for("c.example").expect("mint c");
        assert_eq!(
            resolver.lock_cache().len(),
            2,
            "cache never exceeds its cap"
        );

        let a3 = resolver.cert_for("a.example").expect("a still cached");
        assert!(Arc::ptr_eq(&a1, &a3), "recently-used a must not be evicted");

        // "b" was evicted, so re-requesting it mints a fresh, distinct key.
        let b2 = resolver.cert_for("b.example").expect("re-mint b");
        assert!(
            !Arc::ptr_eq(&b1, &b2),
            "evicted host must be re-minted, not served stale"
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

    // Issue #593: CaSource::resolve encodes the whole validation matrix in one place.
    #[test]
    fn ca_source_resolve_matrix() {
        use std::path::PathBuf;
        let p = || Some(PathBuf::from("x"));
        let s = || Some("pem".to_string());

        assert!(matches!(
            CaSource::resolve(None, None, None, None).unwrap(),
            CaSource::Generate
        ));
        assert!(matches!(
            CaSource::resolve(p(), p(), None, None).unwrap(),
            CaSource::Paths { .. }
        ));
        assert!(matches!(
            CaSource::resolve(None, None, s(), s()).unwrap(),
            CaSource::Pem { .. }
        ));
        // Half a path pair, half a PEM pair, and both pairs together are all rejected.
        assert!(
            CaSource::resolve(p(), None, None, None).is_err(),
            "half path pair"
        );
        assert!(
            CaSource::resolve(None, None, s(), None).is_err(),
            "half PEM pair"
        );
        assert!(
            CaSource::resolve(p(), p(), s(), s()).is_err(),
            "path and PEM are mutually exclusive"
        );
    }

    #[test]
    fn ca_source_debug_never_prints_inline_pem() {
        let src = CaSource::Pem {
            cert: "-----BEGIN CERTIFICATE-----secret".to_string(),
            key: "-----BEGIN PRIVATE KEY-----supersecret".to_string(),
        };
        let rendered = format!("{src:?}");
        assert!(
            !rendered.contains("secret"),
            "inline PEM must never appear in Debug output"
        );
    }

    #[test]
    fn from_source_pem_round_trips() {
        // Generate a CA, export its PEM pair, and reload it through CaSource::Pem — the loaded CA
        // must expose the same trust anchor and mint leaves that chain to it.
        let original = CertificateAuthority::generate().expect("generate CA");
        let cert_pem = original.ca_cert_pem().to_string();
        let key_pem = original.ca_key_pem();

        let source = CaSource::resolve(None, None, Some(cert_pem.clone()), Some(key_pem))
            .expect("resolve inline PEM");
        let loaded = CertificateAuthority::from_source(&source).expect("load from inline PEM");
        assert_eq!(loaded.ca_cert_pem(), cert_pem, "same trust anchor");
        let ck = loaded
            .mint_leaf("svc.internal")
            .expect("mint via loaded CA");
        assert_chains_to_ca(&loaded, &ck.cert[0], &[], "svc.internal");
    }

    #[test]
    fn ca_key_pem_is_loadable_pkcs8() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let key_pem = ca.ca_key_pem();
        assert!(
            key_pem.contains("PRIVATE KEY"),
            "serialized as PEM private key"
        );
        // The exported key must reload as the same CA (cert PEM equality).
        let reloaded = CertificateAuthority::load_pem(ca.ca_cert_pem(), &key_pem).expect("reload");
        assert_eq!(reloaded.ca_cert_pem(), ca.ca_cert_pem());
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

    // ===== Issue #1274: a CA meant to be kept gets a real validity window and name =====

    #[test]
    fn generate_with_sets_the_name_and_a_validity_window_from_now() {
        let before = time::OffsetDateTime::now_utc();
        let ca = CertificateAuthority::generate_with(&GenerateOptions {
            common_name: "Acme Test CA".to_string(),
            validity: std::time::Duration::from_secs(10 * 86_400),
        })
        .expect("generate");
        let after = time::OffsetDateTime::now_utc();
        let params = CertificateParams::from_ca_cert_der(ca.ca_cert_der()).expect("parse CA");
        assert!(matches!(params.is_ca, IsCa::Ca(_)));
        let cn = params
            .distinguished_name
            .get(&DnType::CommonName)
            .expect("CN");
        assert_eq!(cn, &rcgen::DnValue::Utf8String("Acme Test CA".to_string()));
        // Five minutes back for clock skew, at whole-second precision.
        assert!(params.not_before <= before - time::Duration::minutes(5));
        assert!(
            params.not_before >= before - time::Duration::minutes(5) - time::Duration::seconds(2)
        );
        assert!(params.not_after >= before + time::Duration::days(10) - time::Duration::seconds(2));
        assert!(params.not_after <= after + time::Duration::days(10));
        // And it still mints leaves that chain to it.
        let ck = ca.mint_leaf("cdn.example.com").expect("mint");
        assert_chains_to_ca(&ca, &ck.cert[0], &[], "cdn.example.com");
    }

    #[test]
    fn generate_keeps_the_in_memory_defaults() {
        let ca = CertificateAuthority::generate().expect("generate");
        let params = CertificateParams::from_ca_cert_der(ca.ca_cert_der()).expect("parse CA");
        assert_eq!(
            params.distinguished_name.get(&DnType::CommonName),
            Some(&rcgen::DnValue::Utf8String("Rift Intercept CA".to_string()))
        );
        assert_eq!(
            params.not_before.year(),
            1975,
            "the listener's ephemeral CA is unchanged"
        );
    }

    // ===== Issue #1277: leaves carry an Authority Key Identifier (strict X.509 clients) =====

    use x509_parser::extensions::{BasicConstraints as X509BasicConstraints, ParsedExtension};
    use x509_parser::prelude::{FromDer, X509Certificate};

    struct LeafIds {
        aki: Option<Vec<u8>>,
        ski: Option<Vec<u8>>,
        ca: Option<bool>,
        subject_cn: Option<String>,
    }

    fn leaf_ids(der: &[u8]) -> LeafIds {
        let (_, cert) = X509Certificate::from_der(der).expect("parse leaf DER");
        let mut ids = LeafIds {
            aki: None,
            ski: None,
            ca: None,
            subject_cn: cert
                .subject()
                .iter_common_name()
                .next()
                .and_then(|cn| cn.as_str().ok())
                .map(str::to_string),
        };
        for ext in cert.extensions() {
            match ext.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(aki) => {
                    ids.aki = aki.key_identifier.as_ref().map(|k| k.0.to_vec());
                }
                ParsedExtension::SubjectKeyIdentifier(ski) => ids.ski = Some(ski.0.to_vec()),
                ParsedExtension::BasicConstraints(X509BasicConstraints { ca, .. }) => {
                    ids.ca = Some(*ca);
                }
                _ => {}
            }
        }
        ids
    }

    #[test]
    fn minted_leaf_carries_aki_matching_ca_ski() {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let ca_ski = leaf_ids(ca.ca_cert_der())
            .ski
            .expect("the CA carries an SKI");
        let ck = ca.mint_leaf("cdn.example.com").expect("mint leaf");
        let leaf = leaf_ids(&ck.cert[0]);
        assert_eq!(
            leaf.aki.as_deref(),
            Some(ca_ski.as_slice()),
            "the leaf's AKI must name the CA's SKI (RFC 5280 4.2.1.1; Python 3.13+ strict mode)"
        );
        assert!(leaf.ski.is_some(), "the leaf carries its own SKI");
        assert_eq!(leaf.ca, Some(false), "the leaf says CA:FALSE explicitly");
        assert_eq!(
            leaf.subject_cn.as_deref(),
            Some("cdn.example.com"),
            "the subject names the host, not rcgen's default"
        );
    }

    /// A loaded CA whose SKI is not rcgen's own SHA-256 derivation: the leaf's AKI must still equal
    /// that CA's SKI — i.e. it is carried over from the certificate, not recomputed from the key.
    #[test]
    fn loaded_ca_leaf_aki_equals_the_original_ski() {
        let ski = vec![0xAB; 20];
        let key = KeyPair::generate().expect("CA key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "Operator CA");
        params.key_identifier_method = rcgen::KeyIdMethod::PreSpecified(ski.clone());
        let cert = params.self_signed(&key).expect("self-sign");
        assert_eq!(leaf_ids(cert.der()).ski.as_deref(), Some(ski.as_slice()));

        let ca = CertificateAuthority::load_pem(&cert.pem(), &key.serialize_pem()).expect("load");
        let ck = ca.mint_leaf("api.example.com").expect("mint leaf");
        assert_eq!(leaf_ids(&ck.cert[0]).aki, Some(vec![0xAB; 20]));
        assert_chains_to_ca(&ca, &ck.cert[0], &[], "api.example.com");
    }

    /// The check that models the failing client: OpenSSL's strict verifier, which is what Python's
    /// `ssl` enforces by default since 3.13. Skipped (with a note) when no OpenSSL is on PATH.
    #[test]
    fn minted_leaf_verifies_under_openssl_x509_strict() {
        let openssl = std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("OpenSSL "));
        if !openssl {
            eprintln!("skipping: OpenSSL not on PATH (LibreSSL does not enforce AKI)");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = CertificateAuthority::generate().expect("generate CA");
        let ck = ca.mint_leaf("cdn.example.com").expect("mint leaf");
        let leaf_pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64_lines(&ck.cert[0])
        );
        let ca_path = dir.path().join("ca.pem");
        let leaf_path = dir.path().join("leaf.pem");
        std::fs::write(&ca_path, ca.ca_cert_pem()).expect("write CA");
        std::fs::write(&leaf_path, leaf_pem).expect("write leaf");
        let out = std::process::Command::new("openssl")
            .args(["verify", "-x509_strict", "-CAfile"])
            .arg(&ca_path)
            .arg(&leaf_path)
            .output()
            .expect("run openssl verify");
        assert!(
            out.status.success(),
            "openssl verify -x509_strict rejected the leaf: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn base64_lines(der: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        b64.as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ===== Issue #1293: a CA pair named by environment variables =====

    fn env_of(
        vars: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    fn named(cert: &str, key: &str) -> CaInputs {
        CaInputs {
            cert_pem_env: Some(cert.to_string()),
            key_pem_env: Some(key.to_string()),
            ..CaInputs::default()
        }
    }

    #[test]
    fn an_env_named_pair_resolves_to_the_variables_pem() {
        let lookup = env_of(&[("CA_C", "cert-pem"), ("CA_K", "key-pem")]);
        match CaSource::from_inputs_with_env(named("CA_C", "CA_K"), &lookup).expect("resolves") {
            CaSource::Pem { cert, key } => {
                assert_eq!(cert, "cert-pem");
                assert_eq!(key, "key-pem");
            }
            other => panic!("expected Pem, got {other:?}"),
        }
    }

    #[test]
    fn an_env_named_pair_is_both_or_neither_and_exclusive() {
        let lookup = env_of(&[("CA_C", "c"), ("CA_K", "k")]);
        let half = CaInputs {
            cert_pem_env: Some("CA_C".into()),
            ..CaInputs::default()
        };
        let err = CaSource::from_inputs_with_env(half, &lookup).expect_err("half pair");
        assert_eq!(
            err.to_string(),
            "intercept CA cert and key env var names must be provided together (or both omitted)"
        );

        let with_inline = CaInputs {
            cert_pem: Some("c".into()),
            key_pem: Some("k".into()),
            ..named("CA_C", "CA_K")
        };
        let with_paths = CaInputs {
            cert_path: Some("c.pem".into()),
            key_path: Some("k.pem".into()),
            ..named("CA_C", "CA_K")
        };
        for inputs in [with_inline, with_paths] {
            let err = CaSource::from_inputs_with_env(inputs, &lookup).expect_err("two sources");
            assert_eq!(
                err.to_string(),
                "intercept CA path, inline PEM and env-named PEM are mutually exclusive — supply only one"
            );
        }
    }

    #[test]
    fn an_unset_or_non_unicode_variable_is_named_and_never_defaults() {
        let lookup = env_of(&[("CA_K", "k")]);
        let err = CaSource::from_inputs_with_env(named("CA_C", "CA_K"), &lookup)
            .expect_err("cert variable unset");
        assert_eq!(
            err.to_string(),
            "environment variable CA_C (named by caCertPemEnv) is not set"
        );
        let not_unicode = |name: &str| match name {
            "CA_C" => Ok("c".to_string()),
            _ => Err(std::env::VarError::NotUnicode(std::ffi::OsString::from(
                "x",
            ))),
        };
        let err = CaSource::from_inputs_with_env(named("CA_C", "CA_K"), &not_unicode)
            .expect_err("key variable not unicode");
        assert_eq!(
            err.to_string(),
            "environment variable CA_K (named by caKeyPemEnv) is not valid UTF-8"
        );
    }

    #[test]
    fn an_env_named_pair_is_not_a_generated_ca() {
        let lookup = env_of(&[("CA_C", "c"), ("CA_K", "k")]);
        let source = CaSource::from_inputs_with_env(named("CA_C", "CA_K"), &lookup).unwrap();
        assert!(!source.is_generate(), "returnCaKey must stay refused");
    }

    #[test]
    fn ca_inputs_debug_never_prints_inline_pem() {
        let inputs = CaInputs {
            key_pem: Some("-----BEGIN PRIVATE KEY-----supersecret".into()),
            cert_pem: Some("-----BEGIN CERTIFICATE-----secret".into()),
            key_pem_env: Some("CA_K".into()),
            ..CaInputs::default()
        };
        let rendered = format!("{inputs:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(
            rendered.contains("CA_K"),
            "env var names are shown: {rendered}"
        );
    }

    #[test]
    fn the_four_argument_resolve_is_unchanged() {
        let err = CaSource::resolve(
            Some("c".into()),
            Some("k".into()),
            Some("c".into()),
            Some("k".into()),
        )
        .expect_err("two sources");
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }
}
