//! Truststore export for the intercept CA (epic #394, slice 2/5).
//!
//! Given the intercept [`CertificateAuthority`], emit trust material a SUT can point at so it
//! trusts the proxy without any crypto committed to its own repo:
//! - the CA certificate as PEM ([`ca_pem`]),
//! - a **PKCS#12** truststore ([`export_pkcs12`]),
//! - a **JKS** truststore for JVM SUTs ([`export_jks`]).
//!
//! Both stores carry a single trusted-certificate entry (the CA) and are integrity-protected by
//! the supplied password — no private keys are included, since a truststore must not hold one.
//! The `*_many` variants (issue #1274) write several trusted entries, so the offline
//! `rift intercept-ca export` can merge the CA with a public-root bundle: a JVM pointed at a store
//! holding the intercept CA alone loses every public root.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::pki_types::CertificateDer;

use p12::{
    CertBag, ContentInfo, MacData, OtherAttribute, PFX, PKCS12Attribute, SafeBag, SafeBagKind,
};
use yasna::models::ObjectIdentifier;

use super::intercept_ca::CertificateAuthority;

/// Alias/friendly-name given to the single CA entry in an exported truststore.
const CA_ALIAS: &str = "rift-intercept-ca";

/// Oracle JDK's "trusted key usage" attribute OID (`2.16.840.1.113894.746875.1.1`). The JVM's
/// PKCS#12 `KeyStore` surfaces a cert bag as a `trustedCertEntry` only when it carries this marker
/// (the one `keytool -importcert` writes); without it Java loads the file but exposes zero trust
/// anchors and TLS validation fails with an empty-`trustAnchors` error (#417).
const ORACLE_TRUSTED_KEY_USAGE_OID: &[u64] = &[2, 16, 840, 1, 113_894, 746_875, 1, 1];

/// `anyExtendedKeyUsage` (`2.5.29.37.0`) — the trusted-key-usage value that marks the CA as trusted
/// for every purpose, matching what `keytool` records for an imported trusted certificate.
const ANY_EXTENDED_KEY_USAGE_OID: &[u64] = &[2, 5, 29, 37, 0];

/// A truststore password. Its `Debug`/`Display` never render the secret so it cannot leak into
/// logs or error messages.
#[derive(Clone)]
pub struct TrustStorePassword(String);

impl TrustStorePassword {
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for TrustStorePassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrustStorePassword(***)")
    }
}

impl std::fmt::Display for TrustStorePassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// The CA certificate as PEM (the trust anchor an operator can also import manually).
pub fn ca_pem(ca: &CertificateAuthority) -> String {
    ca.ca_cert_pem().to_string()
}

/// One trusted certificate in an exported store, under an alias both formats can hold: non-empty,
/// lower-case (the JVM's JKS lowercases aliases on lookup, so an upper-case one is unreachable),
/// no NUL, at most `u16::MAX` bytes, and only BMP characters (where Java's modified UTF-8 and
/// UTF-8 agree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustEntry {
    alias: String,
    der: CertificateDer<'static>,
}

impl TrustEntry {
    pub fn new(alias: impl Into<String>, der: CertificateDer<'static>) -> anyhow::Result<Self> {
        let alias = alias.into();
        anyhow::ensure!(!alias.is_empty(), "truststore alias must not be empty");
        anyhow::ensure!(
            alias.len() <= usize::from(u16::MAX),
            "truststore alias is longer than {} bytes",
            u16::MAX
        );
        anyhow::ensure!(
            !alias.chars().any(char::is_uppercase),
            "truststore alias {alias:?} must be lower-case (JKS lowercases aliases on lookup)"
        );
        anyhow::ensure!(
            alias.chars().all(|c| c != '\0' && u32::from(c) <= 0xFFFF),
            "truststore alias {alias:?} holds a character a JKS alias cannot encode"
        );
        Ok(Self { alias, der })
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn der(&self) -> &CertificateDer<'static> {
        &self.der
    }
}

/// Refuse an empty store or one with a repeated alias: JKS keys entries by alias, so a duplicate
/// would silently shadow a certificate.
fn check_entries(entries: &[TrustEntry]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !entries.is_empty(),
        "a truststore needs at least one certificate"
    );
    let mut seen = HashSet::new();
    for entry in entries {
        anyhow::ensure!(
            seen.insert(entry.alias.as_str()),
            "truststore alias {:?} appears twice",
            entry.alias
        );
    }
    Ok(())
}

/// Every certificate in a PEM bundle (`/etc/ssl/certs/ca-certificates.crt` and the like), in
/// order, each checked to be a parseable X.509 certificate. Text between blocks is ignored. Any
/// other PEM block — `TRUSTED CERTIFICATE` (OpenSSL's trust format), a key, a CRL — is an error,
/// as are a malformed block and a bundle with no certificate: each would otherwise produce a store
/// that trusts less than the operator asked for, or one the SUT's loader rejects whole.
pub fn parse_pem_bundle(pem: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let mut reader = pem.as_bytes();
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("parse PEM bundle: {e}"))?;
    anyhow::ensure!(!certs.is_empty(), "no certificate found in the PEM bundle");
    let blocks = pem
        .lines()
        .filter(|line| line.trim_start().starts_with("-----BEGIN "))
        .count();
    anyhow::ensure!(
        blocks == certs.len(),
        "the PEM bundle holds {blocks} blocks but only {} are plain `CERTIFICATE` blocks; \
         other block types (e.g. `TRUSTED CERTIFICATE`) are not supported",
        certs.len()
    );
    for (index, der) in certs.iter().enumerate() {
        parse_certificate(der)
            .map_err(|e| anyhow::anyhow!("certificate #{} in the PEM bundle: {e}", index + 1))?;
    }
    Ok(certs)
}

fn parse_certificate<'a>(
    der: &'a CertificateDer<'_>,
) -> anyhow::Result<x509_parser::certificate::X509Certificate<'a>> {
    use x509_parser::prelude::FromDer;
    x509_parser::certificate::X509Certificate::from_der(der.as_ref())
        .map(|(_, cert)| cert)
        .map_err(|e| anyhow::anyhow!("not a valid X.509 certificate: {e}"))
}

/// Longest alias derived from a subject CN; a CN is at most 64 characters by RFC 5280, but nothing
/// stops a bundle from carrying a longer one.
const MAX_DERIVED_ALIAS_CHARS: usize = 128;

/// The intercept CA under `ca_alias`, followed by each `bundle` certificate named by its subject
/// CN, lower-cased, made JKS-safe and de-duplicated with a `-2`, `-3`… suffix (issue #1274). A
/// certificate already in the store (the CA itself, or a root repeated across bundles) is kept
/// once. Every certificate must parse as X.509.
pub fn trust_entries_with_bundle(
    ca_alias: &str,
    ca_der: CertificateDer<'static>,
    bundle: Vec<CertificateDer<'static>>,
) -> anyhow::Result<Vec<TrustEntry>> {
    parse_certificate(&ca_der).map_err(|e| anyhow::anyhow!("the CA certificate: {e}"))?;
    let mut seen_der: HashSet<Vec<u8>> = HashSet::from([ca_der.to_vec()]);
    let mut entries = vec![TrustEntry::new(ca_alias, ca_der)?];
    let mut taken: HashSet<String> = HashSet::from([ca_alias.to_string()]);
    for der in bundle {
        if !seen_der.insert(der.to_vec()) {
            continue;
        }
        let base = subject_alias(&der)?;
        let mut alias = base.clone();
        let mut n = 2;
        while !taken.insert(alias.clone()) {
            alias = format!("{base}-{n}");
            n += 1;
        }
        entries.push(TrustEntry::new(alias, der)?);
    }
    Ok(entries)
}

/// A bundle certificate's alias: its subject CN lower-cased, with anything a JKS alias cannot hold
/// replaced, capped at [`MAX_DERIVED_ALIAS_CHARS`]. A certificate that does not parse is an error;
/// one that parses but has no usable CN is still trusted — the name is only a label — as `cert`.
fn subject_alias(der: &CertificateDer<'_>) -> anyhow::Result<String> {
    let cert = parse_certificate(der)?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or_default();
    let alias: String = cn
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| {
            if c == '\0' || u32::from(c) > 0xFFFF || c.is_uppercase() {
                '_'
            } else {
                c
            }
        })
        .take(MAX_DERIVED_ALIAS_CHARS)
        .collect();
    let alias = alias.trim();
    Ok(if alias.is_empty() {
        "cert".to_string()
    } else {
        alias.to_string()
    })
}

/// Export a PKCS#12 truststore containing the CA certificate as a single trusted entry,
/// integrity-protected by `password`.
pub fn export_pkcs12(
    ca: &CertificateAuthority,
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    export_pkcs12_many(
        &[TrustEntry::new(CA_ALIAS, ca.ca_cert_der().clone())?],
        password,
    )
}

/// Export a PKCS#12 truststore with one trusted cert bag per entry, in order, each carrying the
/// JVM's trusted-key-usage marker (#417) and its alias as the friendly name.
pub fn export_pkcs12_many(
    entries: &[TrustEntry],
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    check_entries(entries)?;
    let certs: Vec<(String, Vec<u8>)> = entries
        .iter()
        .map(|e| (e.alias.clone(), e.der.as_ref().to_vec()))
        .collect();
    let bmp = bmp_string(password.as_str());

    // p12's `MacData::new` obtains its MAC salt via `getrandom().unwrap()`, which panics if the
    // OS RNG is unavailable (seccomp-restricted sandbox, very early boot). Contain that panic and
    // surface it as a typed error so this `Result`-returning function never unwinds into callers.
    std::panic::catch_unwind(move || {
        // The trusted-key-usage attribute value is a SET containing the anyExtendedKeyUsage OID.
        // `data` holds DER-encoded values; p12 wraps them in the SET when writing the attribute.
        let trusted_key_usage = PKCS12Attribute::Other(OtherAttribute {
            oid: ObjectIdentifier::from_slice(ORACLE_TRUSTED_KEY_USAGE_OID),
            data: vec![yasna::construct_der(|w| {
                w.write_oid(&ObjectIdentifier::from_slice(ANY_EXTENDED_KEY_USAGE_OID));
            })],
        });
        let cert_bags: Vec<SafeBag> = certs
            .into_iter()
            .map(|(alias, der)| SafeBag {
                bag: SafeBagKind::CertBag(CertBag::X509(der)),
                attributes: vec![
                    PKCS12Attribute::FriendlyName(alias),
                    trusted_key_usage.clone(),
                ],
            })
            .collect();
        // SafeContents ::= SEQUENCE OF SafeBag
        let safe_contents = yasna::construct_der(|w| {
            w.write_sequence_of(|w| {
                for bag in &cert_bags {
                    bag.write(w.next());
                }
            });
        });
        // AuthenticatedSafe ::= SEQUENCE OF ContentInfo — one unencrypted Data holding the certs.
        let auth_safe = yasna::construct_der(|w| {
            w.write_sequence_of(|w| ContentInfo::Data(safe_contents).write(w.next()));
        });
        let mac_data = MacData::new(&auth_safe, &bmp);
        let pfx = PFX {
            version: 3,
            auth_safe: ContentInfo::Data(auth_safe),
            mac_data: Some(mac_data),
        };
        pfx.to_der()
    })
    .map_err(|payload| {
        // Preserve the real panic message so an RNG failure (getrandom) is not conflated with an
        // encoding failure (e.g. a malformed OID constant, which `write_oid` panics on).
        let cause = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic (system RNG unavailable?)");
        anyhow::anyhow!("failed to build PKCS#12 truststore: {cause}")
    })
}

/// Export a JKS truststore containing the CA certificate as a single `trustedCertEntry`,
/// integrity-protected by `password`. Hand-encoded per the JKS format (there is no maintained
/// pure-Rust writer); only the cert-only subset is produced.
pub fn export_jks(
    ca: &CertificateAuthority,
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    export_jks_many(
        &[TrustEntry::new(CA_ALIAS, ca.ca_cert_der().clone())?],
        password,
    )
}

/// Export a JKS truststore with one `trustedCertEntry` per entry, in order.
pub fn export_jks_many(
    entries: &[TrustEntry],
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    const MAGIC: u32 = 0xFEED_FEED;
    const VERSION: u32 = 2;
    const TAG_TRUSTED_CERT: u32 = 2;

    check_entries(entries)?;
    let millis = unix_millis();
    let count =
        u32::try_from(entries.len()).map_err(|_| anyhow::anyhow!("too many truststore entries"))?;

    let mut body = Vec::new();
    body.extend_from_slice(&MAGIC.to_be_bytes());
    body.extend_from_slice(&VERSION.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    for entry in entries {
        let der = entry.der.as_ref();
        let len = u32::try_from(der.len())
            .map_err(|_| anyhow::anyhow!("certificate {:?} is too large", entry.alias))?;
        body.extend_from_slice(&TAG_TRUSTED_CERT.to_be_bytes());
        write_jks_utf(&mut body, &entry.alias);
        body.extend_from_slice(&millis.to_be_bytes());
        write_jks_utf(&mut body, "X.509");
        body.extend_from_slice(&len.to_be_bytes());
        body.extend_from_slice(der);
    }

    // Store integrity digest: SHA1( passwordUTF16BE || "Mighty Aphrodite" || body ).
    let mut pre = utf16be(password.as_str());
    pre.extend_from_slice(b"Mighty Aphrodite");
    pre.extend_from_slice(&body);

    let mut out = body;
    out.extend_from_slice(&sha1(&pre));
    Ok(out)
}

/// Java modified-UTF-8 string with a big-endian u16 length prefix. [`TrustEntry`] admits only
/// NUL-free BMP aliases of at most `u16::MAX` bytes, for which modified UTF-8 and UTF-8 coincide.
fn write_jks_utf(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    debug_assert!(bytes.len() <= u16::MAX as usize, "JKS UTF string too long");
    buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn utf16be(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// PKCS#12 BMPString: UTF-16BE plus a two-byte null terminator. Must match p12's internal
/// `bmp_string` so the MAC verifies.
fn bmp_string(s: &str) -> Vec<u8> {
    let mut bytes = utf16be(s);
    bytes.push(0);
    bytes.push(0);
    bytes
}

fn sha1(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data)
        .as_ref()
        .to_vec()
}

fn unix_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => {
            tracing::warn!("system clock is before UNIX_EPOCH; using 0 for JKS entry timestamp");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ca() -> CertificateAuthority {
        CertificateAuthority::generate().expect("generate CA")
    }

    #[test]
    fn ca_pem_equals_ca_certificate() {
        let ca = test_ca();
        assert_eq!(ca_pem(&ca), ca.ca_cert_pem());
        assert!(ca_pem(&ca).starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn pkcs12_round_trips_and_rejects_wrong_password() {
        let ca = test_ca();
        let pw = TrustStorePassword::new("changeit");
        let der = export_pkcs12(&ca, &pw).expect("export pkcs12");

        let pfx = PFX::parse(&der).expect("parse pkcs12");
        assert!(
            pfx.verify_mac("changeit"),
            "MAC must verify with the password"
        );
        assert!(
            !pfx.verify_mac("wrong-password"),
            "MAC must fail with a wrong password"
        );

        let certs = pfx.cert_bags("changeit").expect("read cert bags");
        assert_eq!(certs.len(), 1, "exactly one trusted cert");
        assert_eq!(
            &certs[0],
            ca.ca_cert_der().as_ref(),
            "the stored cert is the CA cert"
        );
    }

    #[test]
    fn pkcs12_cert_bag_carries_oracle_trusted_key_usage() {
        let ca = test_ca();
        let der = export_pkcs12(&ca, &TrustStorePassword::new("changeit")).expect("export pkcs12");

        let pfx = PFX::parse(&der).expect("parse pkcs12");
        let bags = pfx.bags("changeit").expect("read safe bags");
        let cert_bag = bags
            .iter()
            .find(|b| matches!(b.bag, SafeBagKind::CertBag(_)))
            .expect("a cert bag is present");

        // Pin the exact marker the JVM reads with literals independent of the production
        // constants, so a typo in `ORACLE_TRUSTED_KEY_USAGE_OID`/`ANY_EXTENDED_KEY_USAGE_OID` is
        // caught here: OID 2.16.840.1.113894.746875.1.1, value = DER of the anyExtendedKeyUsage
        // OID 2.5.29.37.0 (`06 04 55 1D 25 00`).
        let expected_oid = ObjectIdentifier::from_slice(&[2, 16, 840, 1, 113_894, 746_875, 1, 1]);
        let expected_value: &[u8] = &[0x06, 0x04, 0x55, 0x1D, 0x25, 0x00];

        let trusted_usage = cert_bag
            .attributes
            .iter()
            .find_map(|attr| match attr {
                PKCS12Attribute::Other(other) if other.oid == expected_oid => Some(other),
                _ => None,
            })
            .expect(
                "cert bag must carry the Oracle TrustedKeyUsage attribute so a JVM PKCS#12 \
                 KeyStore surfaces the CA as a trustedCertEntry (>=1 trust anchor)",
            );
        assert_eq!(
            trusted_usage.data,
            vec![expected_value.to_vec()],
            "trusted-key-usage value must be anyExtendedKeyUsage (trusted for all purposes)"
        );

        // The friendly-name alias must remain alongside the trust marker (appended, not replaced).
        assert!(
            cert_bag
                .attributes
                .iter()
                .any(|a| matches!(a, PKCS12Attribute::FriendlyName(n) if n == CA_ALIAS)),
            "FriendlyName alias must coexist with the trust marker"
        );
    }

    #[test]
    fn jks_has_valid_structure_and_digest() {
        let ca = test_ca();
        let pw = TrustStorePassword::new("changeit");
        let bytes = export_jks(&ca, &pw).expect("export jks");

        // Split trailing 20-byte SHA-1 digest from the body.
        assert!(bytes.len() > 20, "jks must have body + digest");
        let (body, digest) = bytes.split_at(bytes.len() - 20);

        // Recompute the store digest and compare.
        let mut pre = utf16be("changeit");
        pre.extend_from_slice(b"Mighty Aphrodite");
        pre.extend_from_slice(body);
        assert_eq!(sha1(&pre), digest, "store integrity digest must match");
        // A wrong password must NOT reproduce the digest.
        let mut wrong = utf16be("nope");
        wrong.extend_from_slice(b"Mighty Aphrodite");
        wrong.extend_from_slice(body);
        assert_ne!(sha1(&wrong), digest, "wrong password must not match digest");

        // Walk the body and confirm magic/version/one trustedCertEntry/CA bytes.
        let mut p = 0usize;
        let read_u32 = |b: &[u8], p: &mut usize| {
            let v = u32::from_be_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let read_utf = |b: &[u8], p: &mut usize| {
            let len = u16::from_be_bytes(b[*p..*p + 2].try_into().unwrap()) as usize;
            *p += 2;
            let s = String::from_utf8(b[*p..*p + len].to_vec()).unwrap();
            *p += len;
            s
        };
        assert_eq!(read_u32(body, &mut p), 0xFEED_FEED, "magic");
        assert_eq!(read_u32(body, &mut p), 2, "version");
        assert_eq!(read_u32(body, &mut p), 1, "entry count");
        assert_eq!(read_u32(body, &mut p), 2, "trustedCertEntry tag");
        assert_eq!(read_utf(body, &mut p), CA_ALIAS, "alias");
        p += 8; // creation timestamp
        assert_eq!(read_utf(body, &mut p), "X.509", "cert type");
        let cert_len = read_u32(body, &mut p) as usize;
        assert_eq!(&body[p..p + cert_len], ca.ca_cert_der().as_ref(), "CA DER");
    }

    #[test]
    fn password_debug_and_display_redact_the_secret() {
        let pw = TrustStorePassword::new("s3cr3t");
        assert!(!format!("{pw:?}").contains("s3cr3t"));
        assert!(!format!("{pw}").contains("s3cr3t"));
    }

    #[test]
    fn edge_case_passwords_round_trip_both_formats() {
        // Empty and non-ASCII passwords exercise the UTF-16BE/BMP encoding paths that a real
        // JVM/OpenSSL consumer relies on.
        for pw in ["", "pä$$wörd-\u{1F510}"] {
            let ca = test_ca();
            let tsp = TrustStorePassword::new(pw);

            let der = export_pkcs12(&ca, &tsp).expect("export pkcs12");
            let pfx = PFX::parse(&der).expect("parse pkcs12");
            assert!(pfx.verify_mac(pw), "pkcs12 MAC verifies for {pw:?}");
            assert_eq!(
                pfx.cert_bags(pw).expect("cert bags")[0],
                ca.ca_cert_der().as_ref()
            );

            let jks = export_jks(&ca, &tsp).expect("export jks");
            let (body, digest) = jks.split_at(jks.len() - 20);
            let mut pre = utf16be(pw);
            pre.extend_from_slice(b"Mighty Aphrodite");
            pre.extend_from_slice(body);
            assert_eq!(sha1(&pre), digest, "jks digest for {pw:?}");
        }
    }

    /// Documents JVM compatibility: `keytool` surfaces the exported PKCS#12 CA as a
    /// `trustedCertEntry` (the #417 fix). Ignored by default because it requires a JDK on the runner.
    #[test]
    #[ignore = "requires keytool (JDK) on PATH"]
    fn pkcs12_is_readable_by_keytool_as_trusted_cert() {
        use std::io::Write;
        use std::process::Command;

        let ca = test_ca();
        let pw = "changeit";
        let bytes = export_pkcs12(&ca, &TrustStorePassword::new(pw)).expect("export pkcs12");
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rift-intercept-{}.p12", std::process::id()));
        std::fs::File::create(&path)
            .and_then(|mut f| f.write_all(&bytes))
            .expect("write p12");

        let out = Command::new("keytool")
            .args([
                "-list",
                "-storetype",
                "PKCS12",
                "-storepass",
                pw,
                "-keystore",
            ])
            .arg(&path)
            .output()
            .expect("run keytool");
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "keytool -list should succeed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(CA_ALIAS),
            "keytool should list the CA alias"
        );
        assert!(
            stdout.contains("trustedCertEntry"),
            "the CA must be surfaced as a trustedCertEntry, not hidden as a supporting cert"
        );
    }

    /// Documents JVM compatibility: `keytool` can list the exported JKS. Ignored by default
    /// because it requires a JDK on the runner.
    #[test]
    #[ignore = "requires keytool (JDK) on PATH"]
    fn jks_is_readable_by_keytool() {
        use std::io::Write;
        use std::process::Command;

        let ca = test_ca();
        let pw = "changeit";
        let bytes = export_jks(&ca, &TrustStorePassword::new(pw)).expect("export jks");
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rift-intercept-{}.jks", std::process::id()));
        std::fs::File::create(&path)
            .and_then(|mut f| f.write_all(&bytes))
            .expect("write jks");

        let out = Command::new("keytool")
            .args(["-list", "-storetype", "JKS", "-storepass", pw, "-keystore"])
            .arg(&path)
            .output()
            .expect("run keytool");
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "keytool -list should succeed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(CA_ALIAS),
            "keytool should list the CA alias"
        );
    }

    /// Documents JVM compatibility of the multi-entry stores (issue #1274): `keytool` lists every
    /// entry of both formats as a trusted certificate. Ignored by default (needs a JDK).
    #[test]
    #[ignore = "requires keytool (JDK) on PATH"]
    fn many_entry_stores_are_readable_by_keytool() {
        use std::process::Command;
        let (a, b) = (second_ca("A"), second_ca("B"));
        let entries = [entry("rift-intercept-ca", &a), entry("isrg root x1", &b)];
        let pw = TrustStorePassword::new("changeit");
        let dir = tempfile::tempdir().expect("tempdir");
        for (kind, bytes) in [
            ("JKS", export_jks_many(&entries, &pw).expect("jks")),
            ("PKCS12", export_pkcs12_many(&entries, &pw).expect("p12")),
        ] {
            let path = dir.path().join(format!("store.{kind}"));
            std::fs::write(&path, bytes).expect("write");
            let out = Command::new("keytool")
                .args([
                    "-list",
                    "-storetype",
                    kind,
                    "-storepass",
                    "changeit",
                    "-keystore",
                ])
                .arg(&path)
                .output()
                .expect("run keytool");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{kind}: {stdout}");
            assert!(stdout.contains("2 entries"), "{kind}: {stdout}");
            for alias in ["rift-intercept-ca", "isrg root x1"] {
                assert!(
                    stdout.contains(&format!("{alias}, ")),
                    "{kind} lists {alias}: {stdout}"
                );
            }
            assert_eq!(
                stdout.matches("trustedCertEntry").count(),
                2,
                "{kind}: {stdout}"
            );
        }
    }

    // ===== Issue #1274: several trusted entries per store, and PEM bundles =====

    fn second_ca(cn: &str) -> CertificateAuthority {
        CertificateAuthority::generate_with(&crate::proxy::intercept_ca::GenerateOptions {
            common_name: cn.to_string(),
            validity: std::time::Duration::from_secs(86_400),
        })
        .expect("generate CA")
    }

    fn entry(alias: &str, ca: &CertificateAuthority) -> TrustEntry {
        TrustEntry::new(alias, ca.ca_cert_der().clone()).expect("valid alias")
    }

    /// Walk a JKS body, returning `(alias, der)` per trusted entry.
    fn jks_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let body = &bytes[..bytes.len() - 20];
        let mut p = 0usize;
        let u32_at = |p: &mut usize| {
            let v = u32::from_be_bytes(body[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        assert_eq!(u32_at(&mut p), 0xFEED_FEED);
        assert_eq!(u32_at(&mut p), 2);
        let count = u32_at(&mut p);
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(&mut p), 2, "trustedCertEntry tag");
            let len = u16::from_be_bytes(body[p..p + 2].try_into().unwrap()) as usize;
            let alias = String::from_utf8(body[p + 2..p + 2 + len].to_vec()).unwrap();
            p += 2 + len + 8;
            let tlen = u16::from_be_bytes(body[p..p + 2].try_into().unwrap()) as usize;
            p += 2 + tlen;
            let clen = u32_at(&mut p) as usize;
            out.push((alias, body[p..p + clen].to_vec()));
            p += clen;
        }
        assert_eq!(p, body.len(), "no trailing bytes after the entries");
        out
    }

    #[test]
    fn jks_many_writes_one_trusted_entry_per_cert() {
        let (a, b) = (second_ca("A"), second_ca("B"));
        let bytes = export_jks_many(
            &[entry("rift-intercept-ca", &a), entry("other root", &b)],
            &TrustStorePassword::new("changeit"),
        )
        .expect("export");
        let entries = jks_entries(&bytes);
        assert_eq!(
            entries,
            vec![
                ("rift-intercept-ca".to_string(), a.ca_cert_der().to_vec()),
                ("other root".to_string(), b.ca_cert_der().to_vec()),
            ]
        );
    }

    #[test]
    fn pkcs12_many_marks_every_cert_trusted() {
        let (a, b) = (second_ca("A"), second_ca("B"));
        let der = export_pkcs12_many(
            &[entry("first", &a), entry("second", &b)],
            &TrustStorePassword::new("changeit"),
        )
        .expect("export");
        let pfx = PFX::parse(&der).expect("parse");
        assert!(pfx.verify_mac("changeit"));
        let bags = pfx.bags("changeit").expect("bags");
        assert_eq!(bags.len(), 2);
        let trusted = ObjectIdentifier::from_slice(&[2, 16, 840, 1, 113_894, 746_875, 1, 1]);
        for (bag, (name, ca)) in bags.iter().zip([("first", &a), ("second", &b)]) {
            assert!(
                matches!(&bag.bag, SafeBagKind::CertBag(CertBag::X509(d)) if d == ca.ca_cert_der().as_ref())
            );
            assert!(
                bag.attributes
                    .iter()
                    .any(|x| matches!(x, PKCS12Attribute::FriendlyName(n) if n == name))
            );
            assert!(
                bag.attributes
                    .iter()
                    .any(|x| matches!(x, PKCS12Attribute::Other(o) if o.oid == trusted))
            );
        }
    }

    #[test]
    fn many_refuses_duplicate_aliases() {
        let a = second_ca("A");
        let pw = TrustStorePassword::new("changeit");
        let dup = [entry("same", &a), entry("same", &a)];
        assert!(export_jks_many(&dup, &pw).is_err());
        assert!(export_pkcs12_many(&dup, &pw).is_err());
        assert!(
            export_jks_many(&[], &pw).is_err(),
            "an empty store is refused"
        );
    }

    #[test]
    fn trust_entry_refuses_an_alias_jks_cannot_hold() {
        let a = second_ca("A");
        assert!(TrustEntry::new("", a.ca_cert_der().clone()).is_err());
        assert!(
            TrustEntry::new("Upper", a.ca_cert_der().clone()).is_err(),
            "JKS aliases are lower-case"
        );
        assert!(TrustEntry::new("nul\0", a.ca_cert_der().clone()).is_err());
    }

    #[test]
    fn single_entry_exports_are_unchanged() {
        let ca = test_ca();
        let pw = TrustStorePassword::new("changeit");
        let entries = jks_entries(&export_jks(&ca, &pw).expect("jks"));
        assert_eq!(
            entries,
            vec![(CA_ALIAS.to_string(), ca.ca_cert_der().to_vec())]
        );
    }

    #[test]
    fn parse_pem_bundle_reads_every_certificate() {
        let (a, b) = (second_ca("A"), second_ca("B"));
        let bundle = format!(
            "# Debian-style comment\r\n{}\r\nsome trailing text\n{}",
            a.ca_cert_pem().replace('\n', "\r\n"),
            b.ca_cert_pem()
        );
        let certs = parse_pem_bundle(&bundle).expect("parse");
        assert_eq!(
            certs,
            vec![a.ca_cert_der().clone(), b.ca_cert_der().clone()]
        );
    }

    #[test]
    fn parse_pem_bundle_refuses_empty_and_broken_input() {
        assert!(parse_pem_bundle("no certificates here\n").is_err());
        let broken = "-----BEGIN CERTIFICATE-----\n!!!notbase64!!!\n-----END CERTIFICATE-----\n";
        assert!(parse_pem_bundle(broken).is_err());
    }

    #[test]
    fn bundle_entries_take_lower_cased_unique_aliases_from_the_subject() {
        let ca = test_ca();
        let (x, y, z) = (
            second_ca("ISRG Root X1"),
            second_ca("ISRG Root X1"),
            second_ca("Rift-Intercept-CA"),
        );
        let entries = trust_entries_with_bundle(
            CA_ALIAS,
            ca.ca_cert_der().clone(),
            vec![
                x.ca_cert_der().clone(),
                y.ca_cert_der().clone(),
                z.ca_cert_der().clone(),
            ],
        )
        .expect("entries");
        let aliases: Vec<&str> = entries.iter().map(TrustEntry::alias).collect();
        assert_eq!(
            aliases,
            vec![
                "rift-intercept-ca",
                "isrg root x1",
                "isrg root x1-2",
                "rift-intercept-ca-2"
            ]
        );
    }

    fn cert_with_cn(cn: Option<&str>) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        if let Some(cn) = cn {
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, cn);
        }
        params.self_signed(&key).expect("self-sign").der().clone()
    }

    #[test]
    fn bundle_entries_name_a_cn_less_cert_and_cap_long_names() {
        let ca = test_ca();
        let long = "R".repeat(300);
        let entries = trust_entries_with_bundle(
            CA_ALIAS,
            ca.ca_cert_der().clone(),
            vec![
                cert_with_cn(None),
                cert_with_cn(None),
                cert_with_cn(Some(&long)),
            ],
        )
        .expect("entries");
        let aliases: Vec<&str> = entries.iter().map(TrustEntry::alias).collect();
        assert_eq!(aliases[1], "cert");
        assert_eq!(aliases[2], "cert-2");
        assert_eq!(aliases[3], "r".repeat(128));
    }

    /// A generated suffix must not collide with a certificate whose own CN already looks like one.
    #[test]
    fn bundle_alias_suffixes_skip_names_already_taken() {
        let ca = test_ca();
        let entries = trust_entries_with_bundle(
            CA_ALIAS,
            ca.ca_cert_der().clone(),
            vec![
                cert_with_cn(Some("X")),
                cert_with_cn(Some("x-2")),
                cert_with_cn(Some("X")),
            ],
        )
        .expect("entries");
        let aliases: Vec<&str> = entries.iter().map(TrustEntry::alias).collect();
        assert_eq!(aliases, vec![CA_ALIAS, "x", "x-2", "x-3"]);
    }

    #[test]
    fn bundle_entries_keep_a_repeated_certificate_once() {
        let ca = test_ca();
        let root = cert_with_cn(Some("Root"));
        let entries = trust_entries_with_bundle(
            CA_ALIAS,
            ca.ca_cert_der().clone(),
            vec![ca.ca_cert_der().clone(), root.clone(), root],
        )
        .expect("entries");
        let aliases: Vec<&str> = entries.iter().map(TrustEntry::alias).collect();
        assert_eq!(
            aliases,
            vec![CA_ALIAS, "root"],
            "the CA and the repeated root appear once"
        );
    }

    #[test]
    fn bundle_entries_refuse_a_block_that_is_not_a_certificate() {
        let ca = test_ca();
        let junk = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(
            trust_entries_with_bundle(CA_ALIAS, ca.ca_cert_der().clone(), vec![junk.clone()])
                .is_err()
        );
        assert!(
            trust_entries_with_bundle(CA_ALIAS, junk, vec![]).is_err(),
            "nor as the CA"
        );
    }

    #[test]
    fn parse_pem_bundle_refuses_other_block_types_and_non_certificates() {
        let ca = test_ca();
        let trusted = ca
            .ca_cert_pem()
            .replace("BEGIN CERTIFICATE", "BEGIN TRUSTED CERTIFICATE")
            .replace("END CERTIFICATE", "END TRUSTED CERTIFICATE");
        let mixed = format!("{}{}", ca.ca_cert_pem(), trusted);
        let err = parse_pem_bundle(&mixed).expect_err("a skipped block is refused");
        assert!(format!("{err}").contains("TRUSTED CERTIFICATE"), "{err}");

        use base64::Engine;
        let not_x509 = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode([0x30, 0x03, 0x02, 0x01, 0x00])
        );
        let err = parse_pem_bundle(&format!("{}{not_x509}", ca.ca_cert_pem()))
            .expect_err("a block that is not X.509 is refused");
        assert!(format!("{err}").contains("certificate #2"), "{err}");
    }

    #[test]
    fn jks_many_digest_is_keyed_by_the_password() {
        let (a, b) = (second_ca("A"), second_ca("B"));
        let bytes = export_jks_many(
            &[entry("a", &a), entry("b", &b)],
            &TrustStorePassword::new("s3cret"),
        )
        .expect("export");
        let (body, digest) = bytes.split_at(bytes.len() - 20);
        let digest_with = |pw: &str| {
            let mut pre = utf16be(pw);
            pre.extend_from_slice(b"Mighty Aphrodite");
            pre.extend_from_slice(body);
            sha1(&pre)
        };
        assert_eq!(digest_with("s3cret"), digest);
        assert_ne!(digest_with("changeit"), digest);
    }
}
