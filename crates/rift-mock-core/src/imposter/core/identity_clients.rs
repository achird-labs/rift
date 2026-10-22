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

use anyhow::anyhow;
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio::sync::watch;
use tracing::warn;

use crate::proxy::client_identity::{IdentityKey, identity_key};
use crate::proxy::{ClientIdentity, OutboundTls};

/// Distinct identities held before the cache is emptied. Far above what a real config carries (one
/// per mTLS upstream); low enough that the clients it holds are cheap to keep.
pub(crate) const MAX_IDENTITY_CLIENTS: usize = 64;

/// How many times a build is retried when the trust policy changes while it runs.
const POLICY_RACE_RETRIES: usize = 3;

/// One trust policy as it was set. Each cached client is tagged with the `Arc` it was built under,
/// so a client built under a policy that has since been replaced is never served or stored, even
/// when [`IdentityClients::set_policy`] lands while that client is being built.
#[derive(Debug, Default)]
struct PolicyState {
    /// `None` until one is set, which means the default policy.
    policy: Option<OutboundTls>,
}

/// How one derivation ended, as every request waiting on it sees it.
#[derive(Clone)]
enum Outcome {
    Client(Arc<reqwest::Client>),
    /// The identity does not decode: cached, since no policy changes that.
    Refused(Arc<str>),
    /// The policy could not be realised (e.g. no trust anchors): not cached, the next request
    /// retries, as the plain client's default does.
    Failed(Arc<str>),
    /// Built under a policy replaced meanwhile: not stored; the waiters build again.
    Stale,
}

#[derive(Default)]
struct Entries {
    clients: HashMap<IdentityKey, (Arc<PolicyState>, Arc<reqwest::Client>)>,
    /// Identities that do not decode, with their refusal (which never quotes the key). Decoding
    /// does not depend on the policy, so a replayed config's bad identity is decoded once, not on
    /// every request it answers.
    refused: HashMap<IdentityKey, Arc<str>>,
    /// The one derivation running per identity, which every request for it waits on. Removed, in
    /// the same critical section that caches its outcome, by the derivation itself.
    inflight: HashMap<IdentityKey, watch::Receiver<Option<Outcome>>>,
}

/// The per-identity clients, and the trust policy they are built under.
#[derive(Default)]
pub(crate) struct IdentityClients {
    policy: ArcSwap<PolicyState>,
    entries: Mutex<Entries>,
    /// Set once the "client injected without its policy" warning has been logged.
    warned_unset_policy: AtomicBool,
    /// Derivations started, for the single-flight tests.
    #[cfg(test)]
    derivations: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for IdentityClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries.lock();
        f.debug_struct("IdentityClients")
            .field("policy", &self.policy.load().policy)
            .field("clients", &entries.clients.len())
            .field("refused", &entries.refused.len())
            .field("inflight", &entries.inflight.len())
            .finish_non_exhaustive()
    }
}

/// Removes an identity's in-flight entry if its derivation ends without recording an outcome (a
/// panic, or a runtime dropping the blocking task unrun), so later requests start a new one
/// instead of waiting on a sender that is gone.
struct InflightGuard {
    clients: Arc<IdentityClients>,
    id: IdentityKey,
    armed: std::cell::Cell<bool>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.armed.get() {
            self.clients.entries.lock().inflight.remove(&self.id);
        }
    }
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
    /// One derivation runs per identity however many requests need it, on the blocking pool:
    /// decrypting a key derives it (capped, see [`ClientIdentity::from_pem`]) and realising a policy
    /// reads the OS trust store, neither of which belongs on an async worker. The derivation is
    /// detached and records its own outcome, so a request that is abandoned while it runs does not
    /// waste it, and a client that opens and abandons requests in a loop starts no more than one.
    ///
    /// `plain_client_injected` says whether the embedder injected a plain upstream client: if it
    /// did but never set the policy, the identity client cannot know that client's trust, uses the
    /// default, and says so once.
    ///
    /// # Errors
    /// The identity does not decode (only a replayed config can carry one that does not: the doors
    /// refuse it), or the trust policy cannot be realised. Neither message carries the key.
    pub(crate) async fn client_for(
        self: &Arc<Self>,
        cert: &str,
        key: &str,
        passphrase: Option<&str>,
        plain_client_injected: bool,
    ) -> anyhow::Result<Arc<reqwest::Client>> {
        let id = identity_key(cert, key, passphrase);
        for _ in 0..POLICY_RACE_RETRIES {
            let state = self.policy.load_full();
            let mut derivation = None;
            let mut outcome = {
                let mut entries = self.entries.lock();
                if let Some(reason) = entries.refused.get(&id) {
                    return Err(anyhow!("{reason}"));
                }
                if let Some((built_under, client)) = entries.clients.get(&id)
                    && Arc::ptr_eq(built_under, &state)
                {
                    return Ok(Arc::clone(client));
                }
                match entries.inflight.get(&id) {
                    Some(running) => running.clone(),
                    None => {
                        let (running, derive) =
                            self.prepare(id, &state, cert, key, passphrase, plain_client_injected);
                        entries.inflight.insert(id, running.clone());
                        derivation = Some(derive);
                        running
                    }
                }
            };
            // Spawned with the lock released, and only once the entry is registered: tokio drops
            // the closure on this thread when its blocking pool is shutting down, and the guard
            // inside it then takes the (non-reentrant) lock to remove that entry.
            if let Some(derive) = derivation {
                // Detached: dropping the JoinHandle does not cancel a blocking task.
                drop(tokio::task::spawn_blocking(derive));
            }
            let outcome = outcome
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|outcome| outcome.clone());
            match outcome {
                Some(Outcome::Client(client)) => return Ok(client),
                Some(Outcome::Refused(reason) | Outcome::Failed(reason)) => {
                    return Err(anyhow!("{reason}"));
                }
                Some(Outcome::Stale) => {}
                None => {
                    return Err(anyhow!(
                        "the client-certificate build for this proxy did not finish"
                    ));
                }
            }
        }
        Err(anyhow!(
            "the outbound TLS policy kept changing while a client-certificate client was built"
        ))
    }

    /// The derivation for `id` under `state`, ready to spawn, and the receiver its waiters watch.
    /// Called with the entries lock held, so no second derivation for `id` is prepared before this
    /// one is registered; spawned by the caller after the lock is released.
    fn prepare(
        self: &Arc<Self>,
        id: IdentityKey,
        state: &Arc<PolicyState>,
        cert: &str,
        key: &str,
        passphrase: Option<&str>,
        plain_client_injected: bool,
    ) -> (
        watch::Receiver<Option<Outcome>>,
        impl FnOnce() + Send + 'static,
    ) {
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
        #[cfg(test)]
        self.derivations.fetch_add(1, Ordering::Relaxed);
        let (sender, running) = watch::channel(None);
        // Owned copies: the derivation outlives this request. Once per derivation, not per request.
        let (cert, key) = (cert.to_owned(), key.to_owned());
        let passphrase = passphrase.map(str::to_owned);
        let state = Arc::clone(state);
        let guard = InflightGuard {
            clients: Arc::clone(self),
            id,
            armed: std::cell::Cell::new(true),
        };
        let derive = move || {
            let built = build(&cert, &key, passphrase.as_deref(), &state);
            let outcome = guard.clients.finish(id, &state, built);
            guard.armed.set(false);
            // No receiver left is fine: the outcome is already recorded.
            sender.send_replace(Some(outcome));
        };
        (running, derive)
    }

    /// Record a derivation's outcome and end its in-flight entry, in one critical section.
    fn finish(&self, id: IdentityKey, state: &Arc<PolicyState>, built: Outcome) -> Outcome {
        let mut entries = self.entries.lock();
        entries.inflight.remove(&id);
        match built {
            Outcome::Refused(reason) => {
                if entries.refused.len() >= MAX_IDENTITY_CLIENTS {
                    entries.refused.clear();
                }
                entries.refused.insert(id, Arc::clone(&reason));
                Outcome::Refused(reason)
            }
            Outcome::Client(client) => match self.store(&mut entries, id, state, client) {
                Some(client) => Outcome::Client(client),
                None => Outcome::Stale,
            },
            other => other,
        }
    }

    /// Cache `client`, built under `state`, unless the policy has changed since: then `None`, and
    /// nothing is stored. Checked under the entries lock, which `set_policy` takes after swapping
    /// the policy, so a stale client can neither be stored after the swap nor survive its clear.
    fn store(
        &self,
        entries: &mut Entries,
        id: IdentityKey,
        state: &Arc<PolicyState>,
        client: Arc<reqwest::Client>,
    ) -> Option<Arc<reqwest::Client>> {
        if !Arc::ptr_eq(&self.policy.load(), state) {
            return None;
        }
        if entries.clients.len() >= MAX_IDENTITY_CLIENTS && !entries.clients.contains_key(&id) {
            entries.clients.clear();
        }
        entries
            .clients
            .insert(id, (Arc::clone(state), Arc::clone(&client)));
        Some(client)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().clients.len()
    }
}

/// Decode the identity and build its client under `state`'s policy. Blocking.
fn build(cert: &str, key: &str, passphrase: Option<&str>, state: &PolicyState) -> Outcome {
    let identity = match ClientIdentity::from_pem(cert, key, passphrase) {
        Ok(identity) => identity,
        Err(e) => return Outcome::Refused(format!("{e:#}").into()),
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
        Ok(client) => Outcome::Client(client),
        Err(e) => Outcome::Failed(format!("{e:#}").into()),
    }
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
        let clients = Arc::new(IdentityClients::default());
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
        let clients = Arc::new(IdentityClients::default());
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
        let clients = Arc::new(IdentityClients::default());
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
        let clients = Arc::new(IdentityClients::default());
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
        let clients = Arc::new(IdentityClients::default());
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
        let clients = Arc::new(IdentityClients::default());
        clients.set_policy(skip_verify());
        let built_under = clients.policy.load_full();
        let client =
            crate::imposter::core::build_upstream_client(&skip_verify()).expect("a client");
        clients.set_policy(skip_verify());
        let mut entries = clients.entries.lock();
        assert!(
            clients
                .store(&mut entries, [9; 32], &built_under, client)
                .is_none(),
            "a stale client must not be cached"
        );
        drop(entries);
        assert_eq!(clients.len(), 0);
    }

    #[tokio::test]
    async fn a_client_from_an_old_policy_is_never_served() {
        let clients = Arc::new(IdentityClients::default());
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

    const CERT: &str = include_str!("../../proxy/testdata/client-identity/client.crt");
    const PBES2_KEY: &str = include_str!("../../proxy/testdata/client-identity/client.pbes2.key");

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_on_one_identity_derive_it_once() {
        let clients = Arc::new(IdentityClients::default());
        clients.set_policy(skip_verify());
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let clients = Arc::clone(&clients);
            requests.spawn(async move {
                clients
                    .client_for(CERT, PBES2_KEY, Some("rift-test"), false)
                    .await
            });
        }
        let mut served = Vec::new();
        while let Some(result) = requests.join_next().await {
            served.push(result.expect("joined").expect("built"));
        }
        assert_eq!(clients.derivations.load(Ordering::Relaxed), 1);
        assert!(served.iter().all(|c| Arc::ptr_eq(c, &served[0])));
    }

    #[tokio::test]
    async fn an_abandoned_request_still_caches_its_derivation() {
        let clients = Arc::new(IdentityClients::default());
        clients.set_policy(skip_verify());
        {
            let request = clients.client_for(CERT, PBES2_KEY, Some("rift-test"), false);
            tokio::pin!(request);
            // One poll starts the derivation; then the request is dropped, as a closed connection
            // drops its handler.
            std::future::poll_fn(|cx| {
                let _ = request.as_mut().poll(cx);
                std::task::Poll::Ready(())
            })
            .await;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while clients.len() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the derivation was lost"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        clients
            .client_for(CERT, PBES2_KEY, Some("rift-test"), false)
            .await
            .expect("cached");
        assert_eq!(clients.derivations.load(Ordering::Relaxed), 1);
        assert!(clients.entries.lock().inflight.is_empty());
    }

    #[test]
    fn a_runtime_shutting_down_fails_the_request_instead_of_hanging() {
        // tokio drops a blocking closure on the calling thread once its pool is shut down; the
        // guard inside then removes the in-flight entry, which must not need a lock still held.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let handle = runtime.handle().clone();
            runtime.shutdown_background();
            let _context = handle.enter();
            let clients = Arc::new(IdentityClients::default());
            clients.set_policy(skip_verify());
            let request = clients.client_for(CERT, PBES2_KEY, Some("rift-test"), false);
            let mut request = std::pin::pin!(request);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let polled = request.as_mut().poll(&mut cx);
            let failed = matches!(polled, std::task::Poll::Ready(Err(_)));
            let cleared = clients.entries.lock().inflight.is_empty();
            let _ = done.send((failed, cleared));
        });
        let (failed, cleared) = finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("client_for hung on a shut-down runtime");
        assert!(failed, "the request must fail, not wait forever");
        assert!(cleared, "no in-flight entry may be left behind");
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
