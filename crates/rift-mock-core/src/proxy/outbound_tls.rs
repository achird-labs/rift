//! Trust policy for every TLS connection Rift *initiates* (issue #974).
//!
//! Before this module each outbound client decided its own trust independently: the imposter
//! `proxy` stub client and the config-source fetcher carried reqwest's compiled-in webpki roots
//! with no knob at all, while [`super::client`] used the OS store and honoured a per-upstream
//! skip-verify flag. A privately-issued origin — a corporate API gateway — was therefore
//! unreachable from a `proxy` stub no matter how the host was configured, because that client
//! never consulted the OS trust store where the CA lives.
//!
//! [`OutboundTls`] is that decision made once, as data, and shared. It is deliberately plain: the
//! PEM arrives already read, mirroring [`TlsDefaults`](crate::imposter::TlsDefaults), so nothing
//! here touches the filesystem and the "which file, and does it exist" failure stays at the CLI
//! boundary where it belongs.

use std::sync::Arc;

use anyhow::Context;
use rustls::RootCertStore;
use tracing::warn;

use super::tls::NoVerifier;

/// Trust policy for outbound TLS. `Default` means "the OS trust store, verification on", which is
/// what every caller wants outside development.
///
/// The two knobs are not symmetric: `ca_pem` **adds** an anchor and keeps verification intact,
/// while `skip_verify` abandons verification entirely. Prefer the former; the latter exists for
/// development against an origin whose chain is not available.
#[derive(Debug, Clone, Default)]
pub struct OutboundTls {
    /// Extra trust anchor(s) as PEM, appended to the OS trust store.
    ///
    /// Appended rather than replacing, which is the difference that matters in practice:
    /// `SSL_CERT_FILE` (which `rustls-native-certs` honours) *replaces* the store, so pointing it
    /// at a lone private CA silently drops every public root. This keeps both.
    pub ca_pem: Option<String>,
    /// Accept any certificate. Development only — logs a warning whenever a config is built.
    pub skip_verify: bool,
}

impl OutboundTls {
    /// The rustls config this policy describes.
    ///
    /// The returned config leaves `alpn_protocols` **empty, and that is load-bearing**:
    /// `hyper_rustls::HttpsConnectorBuilder::with_tls_config` asserts on it
    /// (`assert!(config.alpn_protocols.is_empty(), "ALPN protocols should not be pre-defined")`)
    /// and hyper-rustls sets ALPN itself from `enable_http2`/`enable_all_versions`. Setting ALPN
    /// here is therefore a runtime panic in [`super::client`], not a preference. Consumers that
    /// need ALPN must add it to their own copy — see [`Self::reqwest_builder`].
    ///
    /// # Errors
    /// - `ca_pem` is malformed, or contains no certificate;
    /// - no trust anchors could be assembled at all (an image with no CA bundle and no `ca_pem`).
    ///
    /// Never panics: this replaces a `.expect` that aborted the process on a TLS/DNS
    /// misconfiguration (the same fix #543 made for [`super::client`]).
    pub fn client_config(&self) -> anyhow::Result<rustls::ClientConfig> {
        // An explicit provider rather than the process default: `install_default()` is called by
        // the binary's `main`, but a unit test or an embedder constructing an `Imposter` directly
        // has never called it, and `ClientConfig::builder()` panics when no default is installed.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .context("building the outbound TLS configuration")?;

        if self.skip_verify {
            warn!(
                "TLS certificate verification DISABLED for outbound connections \
                 (development/testing only)"
            );
            if self.ca_pem.is_some() {
                // Say which setting is void. The generic warning above is easy to read as "the
                // private-CA check is skipped, public roots still apply", which is not what this
                // mode does — and an operator mid-migration may have both set.
                warn!(
                    "an outbound CA was also supplied (--upstream-ca-file / upstreamCaPem); it is \
                     NOT read or used while verification is disabled"
                );
            }
            // `ca_pem` is deliberately not parsed here: nothing is verified against it, so
            // reporting a malformed anchor would be noise about a value with no effect.
            return Ok(builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier::new(&provider)))
                .with_no_client_auth());
        }

        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        // A partially-readable store is normal (macOS keychains routinely hold entries rustls
        // cannot parse), so these are warnings; the emptiness check below is what actually fails.
        for error in &native.errors {
            warn!(%error, "ignoring an unreadable entry in the OS trust store");
        }
        let (added, ignored) = roots.add_parsable_certificates(native.certs);
        if ignored > 0 {
            warn!(added, ignored, "some OS trust anchors were not usable");
        }

        if let Some(pem) = &self.ca_pem {
            let extra: Vec<_> = rustls_pemfile::certs(&mut pem.as_bytes())
                .collect::<Result<_, _>>()
                .context("parsing the outbound CA certificate PEM")?;
            if extra.is_empty() {
                anyhow::bail!(
                    "no certificates found in the outbound CA PEM (--upstream-ca-file / \
                     upstreamCaPem): expected at least one -----BEGIN CERTIFICATE----- block"
                );
            }
            let (_, ignored) = roots.add_parsable_certificates(extra);
            if ignored > 0 {
                anyhow::bail!(
                    "the outbound CA PEM contains {ignored} certificate(s) rustls cannot use"
                );
            }
        }

        if roots.is_empty() {
            anyhow::bail!(
                "no TLS trust anchors available: the OS trust store is empty or unreadable and no \
                 CA PEM was supplied. Install a CA bundle (e.g. ca-certificates), set \
                 SSL_CERT_FILE, or pass --upstream-ca-file"
            );
        }

        Ok(builder.with_root_certificates(roots).with_no_client_auth())
    }

    /// Whether the operator actually asked for anything beyond the default.
    ///
    /// Callers use this to stay lazy: realising the default policy means reading the OS trust
    /// store, and doing that eagerly on every startup delays the first bind for a cost nothing
    /// asked for. A *configured* policy is different — it is realised at startup precisely so a
    /// bad anchor fails there rather than on the first proxied request.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.ca_pem.is_some() || self.skip_verify
    }

    /// The config a reqwest consumer needs: [`Self::client_config`] plus `http/1.1` ALPN.
    ///
    /// reqwest only populates `alpn_protocols` on its own `TlsBackend::Rustls` path; a config
    /// handed to `use_preconfigured_tls` becomes `TlsBackend::BuiltRustls` and is used verbatim.
    /// Without this, these clients would offer no ALPN at all where plain reqwest offered
    /// `http/1.1`, and an origin that requires ALPN would abort the handshake.
    ///
    /// # Errors
    /// Whatever [`Self::client_config`] returns.
    pub fn reqwest_client_config(&self) -> anyhow::Result<rustls::ClientConfig> {
        let mut config = self.client_config()?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }

    /// A reqwest builder carrying this policy.
    ///
    /// # Errors
    /// Whatever [`Self::client_config`] returns.
    pub fn reqwest_builder(&self) -> anyhow::Result<reqwest::ClientBuilder> {
        Ok(reqwest::Client::builder().use_preconfigured_tls(self.reqwest_client_config()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed certificate PEM, generated rather than vendored so nothing expires.
    fn a_certificate_pem() -> String {
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate a test certificate")
            .cert
            .pem()
    }

    #[test]
    fn default_policy_builds_from_the_os_trust_store() {
        // The default carries no CA and does not skip verification: it must still produce a usable
        // config on any host with a CA bundle, which is every host CI runs on.
        let config = OutboundTls::default()
            .client_config()
            .expect("the default policy builds");
        // Empty ALPN is required, not incidental: `HttpsConnectorBuilder::with_tls_config`
        // asserts on it, so pinning ALPN here would panic `create_http_client` at runtime.
        assert!(
            config.alpn_protocols.is_empty(),
            "the shared config must not pin ALPN, got {:?}",
            config.alpn_protocols
        );
    }

    // Issue #974: reqwest populates ALPN only on its own Rustls path, never for a config passed
    // to `use_preconfigured_tls`. Without this the proxy/config-source clients would offer no
    // ALPN where plain reqwest offered `http/1.1`.
    #[test]
    fn the_reqwest_config_offers_http11_alpn() {
        let config = OutboundTls::default()
            .reqwest_client_config()
            .expect("builds");
        assert_eq!(
            config.alpn_protocols,
            vec![b"http/1.1".to_vec()],
            "reqwest consumers must keep offering http/1.1"
        );
    }

    #[tracing_test::traced_test]
    #[test]
    fn skip_verify_names_the_ca_it_is_ignoring() {
        let _ = OutboundTls {
            ca_pem: Some("-----BEGIN CERTIFICATE-----".into()),
            skip_verify: true,
        }
        .client_config();
        assert!(
            logs_contain("NOT read or used"),
            "an ignored CA must be called out by name, not left to inference"
        );
    }

    #[test]
    fn ca_pem_is_accepted_alongside_the_os_roots() {
        let policy = OutboundTls {
            ca_pem: Some(a_certificate_pem()),
            skip_verify: false,
        };
        assert!(
            policy.client_config().is_ok(),
            "a valid CA PEM must build a config"
        );
    }

    #[test]
    fn ca_pem_without_any_certificate_is_an_error() {
        // A private key, no certificate — the classic wrong-half-of-the-pair mistake. It must be a
        // returned error naming the flag, not a silent no-op that leaves the anchor missing.
        let key_only = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate")
            .key_pair
            .serialize_pem();
        let policy = OutboundTls {
            ca_pem: Some(key_only),
            skip_verify: false,
        };
        let err = policy
            .client_config()
            .expect_err("a PEM with no certificate must fail");
        assert!(
            err.to_string().contains("no certificates found"),
            "error must name the problem, got: {err}"
        );
    }

    #[test]
    fn ca_pem_that_is_not_pem_at_all_is_an_error() {
        let policy = OutboundTls {
            ca_pem: Some("clearly not a PEM document".to_string()),
            skip_verify: false,
        };
        let err = policy
            .client_config()
            .expect_err("garbage must not be accepted as a trust anchor");
        assert!(
            err.to_string().contains("no certificates found"),
            "error must name the problem, got: {err}"
        );
    }

    #[test]
    fn empty_ca_pem_is_an_error() {
        let policy = OutboundTls {
            ca_pem: Some(String::new()),
            skip_verify: false,
        };
        assert!(
            policy.client_config().is_err(),
            "an empty CA PEM must not silently mean 'no extra anchor'"
        );
    }

    #[test]
    fn skip_verify_builds_without_consulting_any_trust_store() {
        let policy = OutboundTls {
            ca_pem: None,
            skip_verify: true,
        };
        assert!(
            policy.client_config().is_ok(),
            "skip-verify must not depend on the OS store"
        );
    }

    #[test]
    fn skip_verify_ignores_a_malformed_ca_pem() {
        // Nothing is verified against the anchor in this mode, so a bad value must not fail the
        // build — reporting it would be an error about a setting with no effect.
        let policy = OutboundTls {
            ca_pem: Some("not a pem".to_string()),
            skip_verify: true,
        };
        assert!(
            policy.client_config().is_ok(),
            "skip-verify takes precedence over ca_pem"
        );
    }

    #[test]
    fn is_configured_is_false_only_for_the_untouched_default() {
        assert!(!OutboundTls::default().is_configured());
        assert!(
            OutboundTls {
                ca_pem: Some("x".into()),
                skip_verify: false
            }
            .is_configured()
        );
        assert!(
            OutboundTls {
                ca_pem: None,
                skip_verify: true
            }
            .is_configured()
        );
    }

    #[test]
    fn reqwest_builder_surfaces_the_same_error() {
        let policy = OutboundTls {
            ca_pem: Some("not a pem".to_string()),
            skip_verify: false,
        };
        assert!(
            policy.reqwest_builder().is_err(),
            "the reqwest path must not swallow a policy error"
        );
    }

    #[tracing_test::traced_test]
    #[test]
    fn skip_verify_warns() {
        let _ = OutboundTls {
            ca_pem: None,
            skip_verify: true,
        }
        .client_config();
        assert!(
            logs_contain("verification DISABLED"),
            "disabling verification must be loud"
        );
    }
}
