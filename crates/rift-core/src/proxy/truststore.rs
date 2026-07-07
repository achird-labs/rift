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

use std::time::{SystemTime, UNIX_EPOCH};

use p12::{CertBag, ContentInfo, MacData, PFX, PKCS12Attribute, SafeBag, SafeBagKind};

use super::intercept_ca::CertificateAuthority;

/// Alias/friendly-name given to the single CA entry in an exported truststore.
const CA_ALIAS: &str = "rift-intercept-ca";

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

/// Export a PKCS#12 truststore containing the CA certificate as a single trusted entry,
/// integrity-protected by `password`.
pub fn export_pkcs12(
    ca: &CertificateAuthority,
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    let ca_der = ca.ca_cert_der().as_ref().to_vec();
    let bmp = bmp_string(password.as_str());

    // p12's `MacData::new` obtains its MAC salt via `getrandom().unwrap()`, which panics if the
    // OS RNG is unavailable (seccomp-restricted sandbox, very early boot). Contain that panic and
    // surface it as a typed error so this `Result`-returning function never unwinds into callers.
    std::panic::catch_unwind(move || {
        let cert_bag = SafeBag {
            bag: SafeBagKind::CertBag(CertBag::X509(ca_der)),
            attributes: vec![PKCS12Attribute::FriendlyName(CA_ALIAS.to_string())],
        };
        // SafeContents ::= SEQUENCE OF SafeBag
        let safe_contents = yasna::construct_der(|w| {
            w.write_sequence_of(|w| cert_bag.write(w.next()));
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
    .map_err(|_| anyhow::anyhow!("failed to build PKCS#12 truststore (system RNG unavailable?)"))
}

/// Export a JKS truststore containing the CA certificate as a single `trustedCertEntry`,
/// integrity-protected by `password`. Hand-encoded per the JKS format (there is no maintained
/// pure-Rust writer); only the cert-only subset is produced.
pub fn export_jks(
    ca: &CertificateAuthority,
    password: &TrustStorePassword,
) -> anyhow::Result<Vec<u8>> {
    const MAGIC: u32 = 0xFEED_FEED;
    const VERSION: u32 = 2;
    const TAG_TRUSTED_CERT: u32 = 2;

    let ca_der = ca.ca_cert_der().as_ref();
    let millis = unix_millis();

    let mut body = Vec::new();
    body.extend_from_slice(&MAGIC.to_be_bytes());
    body.extend_from_slice(&VERSION.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // entry count
    body.extend_from_slice(&TAG_TRUSTED_CERT.to_be_bytes());
    write_jks_utf(&mut body, CA_ALIAS);
    body.extend_from_slice(&millis.to_be_bytes());
    write_jks_utf(&mut body, "X.509");
    body.extend_from_slice(&(ca_der.len() as u32).to_be_bytes());
    body.extend_from_slice(ca_der);

    // Store integrity digest: SHA1( passwordUTF16BE || "Mighty Aphrodite" || body ).
    let mut pre = utf16be(password.as_str());
    pre.extend_from_slice(b"Mighty Aphrodite");
    pre.extend_from_slice(&body);

    let mut out = body;
    out.extend_from_slice(&sha1(&pre));
    Ok(out)
}

/// Java modified-UTF-8 string with a big-endian u16 length prefix. ASCII inputs (the only ones
/// used here) coincide with modified UTF-8.
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
}
