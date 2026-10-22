//! Upstream clients that present a `proxy` stub's client certificate (issue #1609).
//!
//! A rustls client config carries at most one client identity, and reqwest builds its TLS config
//! once per client, so each distinct `cert`/`key`/`passphrase` needs a client of its own. They are
//! built lazily, on the first request that needs one, and kept so the connection pool is reused.
//!
//! The cache is keyed by a SHA-256 of the three strings, never by the strings themselves, and is
//! bounded: past [`MAX_IDENTITY_CLIENTS`] distinct identities it is emptied and refilled on demand,
//! so a config that churns identities cannot grow it without limit.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use tracing::warn;

use crate::proxy::{ClientIdentity, OutboundTls};

/// Distinct identities held before the cache is emptied. Far above what a real config carries (one
/// per mTLS upstream); low enough that the clients it holds are cheap to keep.
pub(crate) const MAX_IDENTITY_CLIENTS: usize = 64;

type IdentityKey = [u8; 32];

/// The per-identity clients, and the trust policy they are built under.
#[derive(Default)]
pub(crate) struct IdentityClients {
    /// The operator's outbound trust policy; `None` until one is set, which means the default.
    policy: ArcSwapOption<OutboundTls>,
    clients: Mutex<HashMap<IdentityKey, Arc<reqwest::Client>>>,
    /// Set once the "client injected without its policy" warning has been logged.
    warned_unset_policy: AtomicBool,
}

impl std::fmt::Debug for IdentityClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityClients")
            .field("policy", &self.policy.load())
            .field("clients", &self.clients.lock().len())
            .finish_non_exhaustive()
    }
}

impl IdentityClients {
    /// Build identity clients under `policy` from now on. Clients built under the previous policy
    /// are dropped (in-flight requests keep theirs).
    pub(crate) fn set_policy(&self, policy: OutboundTls) {
        self.policy.store(Some(Arc::new(policy)));
        self.clients.lock().clear();
    }

    /// The client presenting `cert`/`key`/`passphrase`, built on first use.
    ///
    /// `plain_client_injected` says whether the embedder injected a plain upstream client: if it
    /// did but never set the policy, the identity client cannot know that client's trust, uses the
    /// default, and says so once.
    ///
    /// # Errors
    /// The identity does not decode (only a replayed config can carry one that does not: the doors
    /// refuse it), or the trust policy cannot be realised. Neither message carries the key.
    pub(crate) fn client_for(
        &self,
        cert: &str,
        key: &str,
        passphrase: Option<&str>,
        plain_client_injected: bool,
    ) -> anyhow::Result<Arc<reqwest::Client>> {
        let id = identity_key(cert, key, passphrase);
        if let Some(client) = self.clients.lock().get(&id) {
            return Ok(Arc::clone(client));
        }
        // Built outside the lock: realising a policy reads the OS trust store.
        let identity = ClientIdentity::from_pem(cert, key, passphrase)?;
        let policy = self.policy.load_full();
        if policy.is_none()
            && plain_client_injected
            && !self.warned_unset_policy.swap(true, Ordering::Relaxed)
        {
            warn!(
                "a proxy stub presents a client certificate, but only an upstream client was \
                 injected, not its trust policy (ImposterManager::with_outbound_tls); the \
                 client-certificate connection uses the default trust policy"
            );
        }
        let default_policy;
        let policy = match policy.as_deref() {
            Some(policy) => policy,
            None => {
                default_policy = OutboundTls::default();
                &default_policy
            }
        };
        let client = super::build_upstream_client_presenting(policy, Some(&identity))?;
        let mut clients = self.clients.lock();
        if clients.len() >= MAX_IDENTITY_CLIENTS && !clients.contains_key(&id) {
            clients.clear();
        }
        // A racing request may have built the same client first; keep whichever landed.
        Ok(Arc::clone(clients.entry(id).or_insert(client)))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.clients.lock().len()
    }
}

/// SHA-256 over the three strings, each length-prefixed so no two triples share an encoding, and
/// the passphrase's absence distinct from an empty one.
fn identity_key(cert: &str, key: &str, passphrase: Option<&str>) -> IdentityKey {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for part in [Some(cert), Some(key), passphrase] {
        match part {
            Some(text) => {
                digest.update(&[1]);
                digest.update(&text.len().to_be_bytes());
                digest.update(text.as_bytes());
            }
            None => digest.update(&[0]),
        }
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.finish().as_ref());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> (String, String) {
        let mut params = rcgen::CertificateParams::new(vec!["client".to_string()]).expect("params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("cert");
        (cert.pem(), key.serialize_pem())
    }

    fn skip_verify() -> OutboundTls {
        // No OS trust store is read, so these tests do not depend on the host's.
        OutboundTls {
            ca_pem: None,
            skip_verify: true,
        }
    }

    #[test]
    fn one_identity_is_one_client() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        let a = clients.client_for(&cert, &key, None, false).expect("built");
        let b = clients
            .client_for(&cert, &key, None, false)
            .expect("cached");
        assert!(Arc::ptr_eq(&a, &b), "the second request reuses the client");
        assert_eq!(clients.len(), 1);
    }

    #[test]
    fn the_cache_is_bounded() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        // Distinct keys by passphrase: an unencrypted key ignores it, but it is part of the key.
        for i in 0..=MAX_IDENTITY_CLIENTS {
            let passphrase = i.to_string();
            clients
                .client_for(&cert, &key, Some(&passphrase), false)
                .expect("built");
        }
        assert!(
            clients.len() <= MAX_IDENTITY_CLIENTS,
            "{} clients held",
            clients.len()
        );
    }

    #[test]
    fn a_new_policy_drops_the_clients_built_under_the_old_one() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        clients.client_for(&cert, &key, None, false).expect("built");
        clients.set_policy(skip_verify());
        assert_eq!(clients.len(), 0);
    }

    #[test]
    fn an_identity_that_does_not_decode_is_an_error_naming_it() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let err = clients
            .client_for("not a pem", "nor this", None, false)
            .expect_err("refused");
        assert!(format!("{err:#}").contains("proxy.cert"), "{err:#}");
        assert_eq!(clients.len(), 0, "a failure is not cached");
    }

    #[test]
    fn the_key_separates_every_part() {
        assert_ne!(identity_key("ab", "c", None), identity_key("a", "bc", None));
        assert_ne!(
            identity_key("a", "b", None),
            identity_key("a", "b", Some(""))
        );
    }
}
