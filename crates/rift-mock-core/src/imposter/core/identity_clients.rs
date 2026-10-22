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

use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tracing::warn;

use crate::proxy::{ClientIdentity, OutboundTls};

/// Distinct identities held before the cache is emptied. Far above what a real config carries (one
/// per mTLS upstream); low enough that the clients it holds are cheap to keep.
pub(crate) const MAX_IDENTITY_CLIENTS: usize = 64;

/// How many times a build is retried when the trust policy changes while it runs.
const POLICY_RACE_RETRIES: usize = 3;

type IdentityKey = [u8; 32];

/// One trust policy as it was set. Each cached client is tagged with the `Arc` it was built under,
/// so a client built under a policy that has since been replaced is never served or stored, even
/// when [`IdentityClients::set_policy`] lands while that client is being built.
#[derive(Debug, Default)]
struct PolicyState {
    /// `None` until one is set, which means the default policy.
    policy: Option<OutboundTls>,
}

#[derive(Default)]
struct Entries {
    clients: HashMap<IdentityKey, (Arc<PolicyState>, Arc<reqwest::Client>)>,
    /// Identities that do not decode, with their refusal (which never quotes the key). Decoding
    /// does not depend on the policy, so a replayed config's bad identity is decoded once, not on
    /// every request it answers.
    refused: HashMap<IdentityKey, Arc<str>>,
}

/// The per-identity clients, and the trust policy they are built under.
#[derive(Default)]
pub(crate) struct IdentityClients {
    policy: ArcSwap<PolicyState>,
    entries: Mutex<Entries>,
    /// Set once the "client injected without its policy" warning has been logged.
    warned_unset_policy: AtomicBool,
}

impl std::fmt::Debug for IdentityClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries.lock();
        f.debug_struct("IdentityClients")
            .field("policy", &self.policy.load().policy)
            .field("clients", &entries.clients.len())
            .field("refused", &entries.refused.len())
            .finish_non_exhaustive()
    }
}

/// What the blocking build produced.
enum Built {
    Client(Arc<reqwest::Client>),
    /// The identity does not decode: cached, since no policy changes that.
    Refused(Arc<str>),
    /// The policy could not be realised (e.g. no trust anchors): not cached, the next request
    /// retries, as the plain client's default does.
    Failed(anyhow::Error),
}

impl IdentityClients {
    /// Build identity clients under `policy` from now on. Clients built under the previous policy
    /// are dropped (in-flight requests keep theirs), and one still being built is not kept.
    pub(crate) fn set_policy(&self, policy: OutboundTls) {
        self.policy.store(Arc::new(PolicyState {
            policy: Some(policy),
        }));
        self.entries.lock().clients.clear();
    }

    /// The client presenting `cert`/`key`/`passphrase`, built on first use.
    ///
    /// The decode and the build run on the blocking pool: decrypting a key derives it (capped, see
    /// [`ClientIdentity::from_pem`]) and realising a policy reads the OS trust store, neither of
    /// which belongs on an async worker.
    ///
    /// `plain_client_injected` says whether the embedder injected a plain upstream client: if it
    /// did but never set the policy, the identity client cannot know that client's trust, uses the
    /// default, and says so once.
    ///
    /// # Errors
    /// The identity does not decode (only a replayed config can carry one that does not: the doors
    /// refuse it), or the trust policy cannot be realised. Neither message carries the key.
    pub(crate) async fn client_for(
        &self,
        cert: &str,
        key: &str,
        passphrase: Option<&str>,
        plain_client_injected: bool,
    ) -> anyhow::Result<Arc<reqwest::Client>> {
        let id = identity_key(cert, key, passphrase);
        for _ in 0..POLICY_RACE_RETRIES {
            let state = self.policy.load_full();
            {
                let entries = self.entries.lock();
                if let Some(reason) = entries.refused.get(&id) {
                    return Err(anyhow!("{reason}"));
                }
                if let Some((built_under, client)) = entries.clients.get(&id)
                    && Arc::ptr_eq(built_under, &state)
                {
                    return Ok(Arc::clone(client));
                }
            }
            if state.policy.is_none()
                && plain_client_injected
                && !self.warned_unset_policy.swap(true, Ordering::Relaxed)
            {
                warn!(
                    "a proxy stub presents a client certificate, but only an upstream client was \
                     injected, not its trust policy (ImposterManager::with_outbound_tls); the \
                     client-certificate connection uses the default trust policy"
                );
            }
            // Owned copies: the blocking task outlives this borrow. A miss only, never per request.
            let (task_cert, task_key) = (cert.to_owned(), key.to_owned());
            let task_passphrase = passphrase.map(str::to_owned);
            let task_state = Arc::clone(&state);
            let built = tokio::task::spawn_blocking(move || {
                build(
                    &task_cert,
                    &task_key,
                    task_passphrase.as_deref(),
                    &task_state,
                )
            })
            .await
            .context("the client-certificate build task did not finish")?;
            match built {
                Built::Failed(e) => return Err(e),
                Built::Refused(reason) => {
                    let mut entries = self.entries.lock();
                    if entries.refused.len() >= MAX_IDENTITY_CLIENTS {
                        entries.refused.clear();
                    }
                    entries.refused.insert(id, Arc::clone(&reason));
                    return Err(anyhow!("{reason}"));
                }
                Built::Client(client) => {
                    if let Some(client) = self.store(id, &state, client) {
                        return Ok(client);
                    }
                    // The policy changed while this client was built: build again under the new one.
                }
            }
        }
        Err(anyhow!(
            "the outbound TLS policy kept changing while a client-certificate client was built"
        ))
    }

    /// Cache `client`, built under `state`, unless the policy has changed since: then `None`, and
    /// nothing is stored. Checked under the entries lock, which `set_policy` takes after swapping
    /// the policy, so a stale client can neither be stored after the swap nor survive its clear.
    fn store(
        &self,
        id: IdentityKey,
        state: &Arc<PolicyState>,
        client: Arc<reqwest::Client>,
    ) -> Option<Arc<reqwest::Client>> {
        let mut entries = self.entries.lock();
        if !Arc::ptr_eq(&self.policy.load(), state) {
            return None;
        }
        if entries.clients.len() >= MAX_IDENTITY_CLIENTS && !entries.clients.contains_key(&id) {
            entries.clients.clear();
        }
        // A racing request may have built the same client under the same policy; keep the first.
        let entry = entries
            .clients
            .entry(id)
            .and_modify(|entry| {
                if !Arc::ptr_eq(&entry.0, state) {
                    *entry = (Arc::clone(state), Arc::clone(&client));
                }
            })
            .or_insert_with(|| (Arc::clone(state), Arc::clone(&client)));
        Some(Arc::clone(&entry.1))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().clients.len()
    }
}

/// Decode the identity and build its client under `state`'s policy. Blocking.
fn build(cert: &str, key: &str, passphrase: Option<&str>, state: &PolicyState) -> Built {
    let identity = match ClientIdentity::from_pem(cert, key, passphrase) {
        Ok(identity) => identity,
        Err(e) => return Built::Refused(format!("{e:#}").into()),
    };
    let default_policy;
    let policy = match &state.policy {
        Some(policy) => policy,
        None => {
            default_policy = OutboundTls::default();
            &default_policy
        }
    };
    match super::build_upstream_client_presenting(policy, Some(&identity)) {
        Ok(client) => Built::Client(client),
        Err(e) => Built::Failed(e),
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

    #[tokio::test]
    async fn one_identity_is_one_client() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        let a = clients
            .client_for(&cert, &key, None, false)
            .await
            .expect("built");
        let b = clients
            .client_for(&cert, &key, None, false)
            .await
            .expect("cached");
        assert!(Arc::ptr_eq(&a, &b), "the second request reuses the client");
        assert_eq!(clients.len(), 1);
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        // Distinct keys by passphrase: an unencrypted key ignores it, but it is part of the key.
        for i in 0..=MAX_IDENTITY_CLIENTS {
            let passphrase = i.to_string();
            clients
                .client_for(&cert, &key, Some(&passphrase), false)
                .await
                .expect("built");
        }
        assert!(
            clients.len() <= MAX_IDENTITY_CLIENTS,
            "{} clients held",
            clients.len()
        );
    }

    #[tokio::test]
    async fn a_new_policy_drops_the_clients_built_under_the_old_one() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        clients
            .client_for(&cert, &key, None, false)
            .await
            .expect("built");
        clients.set_policy(skip_verify());
        assert_eq!(clients.len(), 0);
    }

    #[tokio::test]
    async fn an_identity_that_does_not_decode_is_refused_once_and_remembered() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        for _ in 0..2 {
            let err = clients
                .client_for("not a pem", "nor this", None, false)
                .await
                .expect_err("refused");
            assert!(format!("{err:#}").contains("proxy.cert"), "{err:#}");
        }
        assert_eq!(clients.len(), 0, "no client is cached");
        assert_eq!(clients.entries.lock().refused.len(), 1, "the refusal is");
    }

    #[tokio::test]
    async fn a_costly_key_is_refused_fast_on_the_request_path() {
        // A replayed config skips admission, so the request path must cap the KDF itself.
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, _) = identity();
        for key in [
            crate::proxy::client_identity::tests::scrypt_key(40, 8, 1),
            crate::proxy::client_identity::tests::pbkdf2_key(4_000_000_000),
        ] {
            for _ in 0..2 {
                let started = std::time::Instant::now();
                let err = clients
                    .client_for(&cert, &key, Some("x"), false)
                    .await
                    .expect_err("refused");
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(1),
                    "took {:?}: {err:#}",
                    started.elapsed()
                );
                assert!(format!("{err:#}").contains("too costly"), "{err:#}");
            }
        }
    }

    #[tokio::test]
    async fn a_client_built_under_a_replaced_policy_is_not_stored() {
        // The race `store` closes: the policy is replaced while a client is being built.
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let built_under = clients.policy.load_full();
        let client =
            crate::imposter::core::build_upstream_client(&skip_verify()).expect("a client");
        clients.set_policy(skip_verify());
        assert!(
            clients.store([9; 32], &built_under, client).is_none(),
            "a stale client must not be cached"
        );
        assert_eq!(clients.len(), 0);
    }

    #[tokio::test]
    async fn a_client_from_an_old_policy_is_never_served() {
        let clients = IdentityClients::default();
        clients.set_policy(skip_verify());
        let (cert, key) = identity();
        let old = clients
            .client_for(&cert, &key, None, false)
            .await
            .expect("built");
        // Put the stale entry back, as a racing build would have before the fix.
        let stale = clients.policy.load_full();
        clients.set_policy(skip_verify());
        clients
            .entries
            .lock()
            .clients
            .insert(identity_key(&cert, &key, None), (stale, Arc::clone(&old)));
        let new = clients
            .client_for(&cert, &key, None, false)
            .await
            .expect("rebuilt");
        assert!(
            !Arc::ptr_eq(&old, &new),
            "the old policy's client was served"
        );
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
