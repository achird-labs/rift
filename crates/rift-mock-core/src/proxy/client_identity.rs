//! The client certificate a `proxy` stub presents to an upstream that requires mutual TLS (issue
//! #1609).
//!
//! Mountebank's proxy hands the `proxy` response's `cert`, `key` and `passphrase` to Node's
//! `https.request`, so an origin that asks for a client certificate gets one. This module turns the
//! same three strings into what rustls presents: a certificate chain and a signing key.
//!
//! The key is a secret the operator supplied. Nothing here puts it, or the passphrase, in an error
//! message, a log line or `Debug` output; the errors name only what is wrong with it.

use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;

const ENCRYPTED_BEGIN: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";
const ENCRYPTED_END: &str = "-----END ENCRYPTED PRIVATE KEY-----";

/// A decoded client identity: the chain to present and the key that signs the handshake.
#[derive(Clone)]
pub struct ClientIdentity {
    certified: Arc<CertifiedKey>,
    /// A `passphrase` was supplied for a key that is not encrypted. Node ignores it, so this is not
    /// an error; the admission check warns about it.
    passphrase_ignored: bool,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately not the key: only how long the chain is.
        f.debug_struct("ClientIdentity")
            .field("chain_len", &self.certified.cert.len())
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// Decode `cert` (PEM, leaf first), `key` (PEM) and the optional `passphrase`.
    ///
    /// Accepted keys: unencrypted PKCS#8, PKCS#1 (`RSA PRIVATE KEY`) and SEC1 (`EC PRIVATE KEY`),
    /// and PKCS#8 `ENCRYPTED PRIVATE KEY` (PBES2: PBKDF2 or scrypt, AES-CBC), which is what
    /// OpenSSL 3 writes. The legacy OpenSSL encryption (`Proc-Type: 4,ENCRYPTED`) is refused with
    /// the command that converts it.
    ///
    /// The key must match the leaf certificate's public key. The leaf is read with x509-parser, so
    /// an X.509 v1 certificate is accepted (issue #1599), as Node accepts it.
    ///
    /// # Errors
    /// Every refusal above, worded without the key or the passphrase.
    pub fn from_pem(cert: &str, key: &str, passphrase: Option<&str>) -> anyhow::Result<Self> {
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow!("proxy.cert is not a readable PEM certificate: {e}"))?;
        if certs.is_empty() {
            bail!(
                "proxy.cert contains no certificate: expected a -----BEGIN CERTIFICATE----- block"
            );
        }

        let (private_key, passphrase_ignored) = decode_key(key, passphrase)?;
        let provider = rustls::crypto::ring::default_provider();
        let certified = super::tls::certified_key(certs, private_key, &provider)
            .context("proxy.key does not pair with proxy.cert")?;
        Ok(Self {
            certified: Arc::new(certified),
            passphrase_ignored,
        })
    }

    /// The certificate chain and signing key, as a rustls client-certificate resolver serves them.
    #[must_use]
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified)
    }

    /// Whether a `passphrase` was supplied for a key that is not encrypted (and so was not used).
    #[must_use]
    pub fn passphrase_ignored(&self) -> bool {
        self.passphrase_ignored
    }
}

/// The private key in `pem`, decrypted with `passphrase` when it is encrypted; and whether a
/// supplied passphrase went unused.
fn decode_key(
    pem: &str,
    passphrase: Option<&str>,
) -> anyhow::Result<(PrivateKeyDer<'static>, bool)> {
    if pem.contains("Proc-Type: 4,ENCRYPTED") {
        bail!(
            "proxy.key uses OpenSSL's legacy encryption (Proc-Type: 4,ENCRYPTED), which Rift does \
             not read; convert it to an encrypted PKCS#8 key with \
             `openssl pkcs8 -topk8 -in key.pem -out key-p8.pem`"
        );
    }
    if let Some(block) = encrypted_block(pem) {
        let Some(passphrase) = passphrase else {
            bail!(
                "proxy.key is encrypted (ENCRYPTED PRIVATE KEY) but no proxy.passphrase was given"
            );
        };
        let (_, document) = pkcs8::Document::from_pem(block)
            .map_err(|e| anyhow!("proxy.key is not a readable encrypted PKCS#8 key: {e}"))?;
        let encrypted = pkcs8::EncryptedPrivateKeyInfoRef::try_from(document.as_bytes())
            .map_err(|e| anyhow!("proxy.key is not a readable encrypted PKCS#8 key: {e}"))?;
        // A wrong passphrase surfaces here: the padding check or the inner decode fails.
        let decrypted = encrypted.decrypt(passphrase).map_err(|_| {
            anyhow!("proxy.passphrase does not decrypt proxy.key (wrong passphrase?)")
        })?;
        let der = PrivatePkcs8KeyDer::from(decrypted.as_bytes().to_vec());
        return Ok((PrivateKeyDer::Pkcs8(der), false));
    }
    let key = rustls_pemfile::private_key(&mut pem.as_bytes())
        .map_err(|e| anyhow!("proxy.key is not a readable PEM private key: {e}"))?
        .ok_or_else(|| {
            anyhow!(
                "proxy.key contains no private key: expected a PKCS#8, RSA or EC PRIVATE KEY block"
            )
        })?;
    Ok((key, passphrase.is_some()))
}

/// The `ENCRYPTED PRIVATE KEY` block in `pem`, delimiters included, without any text around it.
fn encrypted_block(pem: &str) -> Option<&str> {
    let start = pem.find(ENCRYPTED_BEGIN)?;
    let end = pem[start..].find(ENCRYPTED_END)? + start + ENCRYPTED_END.len();
    Some(&pem[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with OpenSSL 3.6.4 (see testdata/client-identity): a P-256 CA, a v3 clientAuth
    // certificate it signed, and that certificate's key in every form `from_pem` must handle.
    // Valid until 2126.
    const CERT: &str = include_str!("testdata/client-identity/client.crt");
    const KEY: &str = include_str!("testdata/client-identity/client.key");
    const PBES2_KEY: &str = include_str!("testdata/client-identity/client.pbes2.key");
    const SCRYPT_KEY: &str = include_str!("testdata/client-identity/client.scrypt.key");
    const LEGACY_KEY: &str = include_str!("testdata/client-identity/client.legacy.key");
    const RSA_CERT: &str = include_str!("testdata/client-identity/client-rsa.crt");
    const RSA_KEY: &str = include_str!("testdata/client-identity/client-rsa.key");
    const PASSPHRASE: &str = "rift-test";
    // Issue #1599's X.509 v1 pair (LibreSSL `x509 -req -signkey`).
    const V1_CERT: &str = include_str!("testdata/x509-v1/server.crt");
    const V1_KEY: &str = include_str!("testdata/x509-v1/server.key");

    fn refusal(cert: &str, key: &str, passphrase: Option<&str>) -> String {
        format!(
            "{:#}",
            ClientIdentity::from_pem(cert, key, passphrase).expect_err("must be refused")
        )
    }

    #[test]
    fn an_unencrypted_pkcs8_key_loads() {
        let id = ClientIdentity::from_pem(CERT, KEY, None).expect("loads");
        assert_eq!(id.certified_key().cert.len(), 1);
        assert!(!id.passphrase_ignored());
    }

    #[test]
    fn a_pkcs1_rsa_key_loads() {
        ClientIdentity::from_pem(RSA_CERT, RSA_KEY, None).expect("PKCS#1 loads");
    }

    #[test]
    fn a_pbes2_encrypted_key_loads_with_its_passphrase() {
        ClientIdentity::from_pem(CERT, PBES2_KEY, Some(PASSPHRASE)).expect("PBES2 key decrypts");
    }

    #[test]
    fn a_scrypt_encrypted_key_loads_with_its_passphrase() {
        ClientIdentity::from_pem(CERT, SCRYPT_KEY, Some(PASSPHRASE)).expect("scrypt key decrypts");
    }

    #[test]
    fn a_wrong_passphrase_is_refused_without_echoing_it() {
        let msg = refusal(CERT, PBES2_KEY, Some("not-the-passphrase"));
        assert!(msg.contains("does not decrypt"), "{msg}");
        assert!(
            !msg.contains("not-the-passphrase"),
            "the passphrase leaked: {msg}"
        );
    }

    #[test]
    fn an_encrypted_key_without_a_passphrase_is_refused() {
        let msg = refusal(CERT, PBES2_KEY, None);
        assert!(msg.contains("no proxy.passphrase"), "{msg}");
    }

    #[test]
    fn a_legacy_encrypted_key_is_refused_naming_the_conversion() {
        let msg = refusal(CERT, LEGACY_KEY, Some(PASSPHRASE));
        assert!(msg.contains("openssl pkcs8 -topk8"), "{msg}");
    }

    #[test]
    fn a_passphrase_on_an_unencrypted_key_is_ignored_not_refused() {
        let id = ClientIdentity::from_pem(CERT, KEY, Some(PASSPHRASE)).expect("loads");
        assert!(id.passphrase_ignored());
    }

    #[test]
    fn a_key_that_is_not_the_certificates_is_refused() {
        let msg = refusal(CERT, RSA_KEY, None);
        assert!(msg.contains("mismatch"), "{msg}");
    }

    #[test]
    fn a_cert_with_no_certificate_is_refused() {
        let msg = refusal("not a pem", KEY, None);
        assert!(msg.contains("contains no certificate"), "{msg}");
    }

    #[test]
    fn a_key_with_no_key_is_refused() {
        let msg = refusal(CERT, CERT, None);
        assert!(msg.contains("contains no private key"), "{msg}");
    }

    #[test]
    fn errors_never_carry_the_key() {
        // The base64 body of the key must not appear in any refusal.
        let body_line = KEY.lines().nth(1).expect("a body line");
        for msg in [
            refusal(RSA_CERT, KEY, None),
            refusal("garbage", KEY, None),
            refusal(CERT, LEGACY_KEY, None),
        ] {
            assert!(!msg.contains(body_line), "the key leaked: {msg}");
        }
    }

    #[test]
    fn an_x509_v1_certificate_and_its_key_load() {
        // rustls' own `keys_match` refuses v1 (#1599); the mbTest fixture is v1.
        ClientIdentity::from_pem(V1_CERT, V1_KEY, None).expect("a v1 pair loads");
    }

    #[test]
    fn debug_does_not_print_the_key() {
        let id = ClientIdentity::from_pem(CERT, KEY, None).expect("loads");
        let shown = format!("{id:?}");
        assert!(!shown.contains(KEY.lines().nth(1).expect("a body line")));
        assert!(shown.starts_with("ClientIdentity"));
    }
}
