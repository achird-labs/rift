use anyhow::{Result, anyhow};
use serde_json::Value;
use std::sync::Arc;

/// Outcome of [`FlowStore::compare_and_set`]: either the write applied, or the key's
/// current value (at decision time) is returned so the caller can react to who won.
#[derive(Debug, Clone, PartialEq)]
pub enum CasOutcome {
    Applied,
    Conflict(Option<Value>),
}

/// Backend-agnostic trait for flow state storage
///
/// This trait is intentionally synchronous to avoid async bridging issues
/// when called from scripts or other synchronous contexts.
/// Redis operations are performed using a blocking client with connection pooling.
pub trait FlowStore: Send + Sync {
    /// Get a value from flow state
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>>;

    /// Set a value in flow state
    fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()>;

    /// Check if a key exists in flow state
    fn exists(&self, flow_id: &str, key: &str) -> Result<bool>;

    /// Delete a key from flow state
    fn delete(&self, flow_id: &str, key: &str) -> Result<()>;

    /// Whether calls to this store block the calling thread on synchronous network I/O. The
    /// request path consults this to decide whether to offload flow-store calls to
    /// `spawn_blocking`, so a slow or pool-exhausted backend can't stall a tokio worker and
    /// head-of-line-block every task multiplexed on it (issue #475). In-memory stores never
    /// block, so the default is `false`; the Redis backend overrides it.
    fn is_blocking(&self) -> bool {
        false
    }

    /// Increment a numeric value (returns new value)
    fn increment(&self, flow_id: &str, key: &str) -> Result<i64>;

    /// Atomically increment a numeric value by `by` (which may be negative), returning the new
    /// value. Starts at 0 when the key is absent, so `increment_by(id, "k", 5)` on an absent key
    /// yields 5 (issue #358).
    ///
    /// The provided default is a NON-ATOMIC get-then-set fallback kept so existing third-party
    /// `FlowStore` impls keep compiling; real backends should override with a genuinely atomic
    /// implementation (see `InMemoryFlowStore`/`RedisFlowStore`).
    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        let current = match self.get(flow_id, key)? {
            Some(Value::Number(n)) if n.is_i64() => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        // `checked_add` so an overflow near i64::MAX errors (fail-loud) instead of panicking in
        // debug / wrapping in release — matching Redis's INCRBY, which also errors on overflow.
        let new_value = current
            .checked_add(by)
            .ok_or_else(|| anyhow!("increment_by overflow: {current} + {by} exceeds i64 range"))?;
        self.set(flow_id, key, Value::Number(new_value.into()))?;
        Ok(new_value)
    }

    /// Set TTL for all keys under a flow_id. `ttl_seconds <= 0` expires (drops) every current key
    /// immediately (issue #530), mirroring the per-key `set_key_ttl` and Redis `EXPIRE` semantics.
    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()>;

    /// Set the TTL of a single `key` under `flow_id` (issue #530). Returns `true` if the key existed
    /// and the TTL was applied, `false` if it was absent — mirroring Redis `EXPIRE`. A
    /// `ttl_seconds <= 0` deletes the key immediately (Redis `EXPIRE` semantics), returning whether
    /// it existed.
    ///
    /// The default is a fail-loud `Err` so third-party `FlowStore` impls keep compiling but can't
    /// silently pretend a per-key TTL was applied when it wasn't (precedent: the #311/#358 defaults).
    fn set_key_ttl(&self, flow_id: &str, key: &str, ttl_seconds: i64) -> Result<bool> {
        let _ = (flow_id, key, ttl_seconds);
        Err(anyhow!("set_key_ttl not supported by this store"))
    }

    /// Remove every key under `flow_id` (issue #530) — the whole-flow invalidation primitive behind
    /// `ctx.state.clear()` and `DELETE /admin/imposters/:port/flow-state/:flow_id`. Clearing an
    /// absent flow is a no-op success (idempotent).
    ///
    /// The default is a fail-loud `Err` so third-party impls keep compiling but can't silently
    /// claim a flow was cleared when it wasn't.
    fn clear_flow(&self, flow_id: &str) -> Result<()> {
        let _ = flow_id;
        Err(anyhow!("clear_flow not supported by this store"))
    }

    /// Atomically set `key` to `new` iff its current value equals `expected`
    /// (`None` = "not present"). Returns the winning current value on conflict.
    ///
    /// Backends may compare by canonical JSON serialization rather than structurally
    /// (the two agree for anything this crate writes; `preserve_order` is off).
    ///
    /// The provided default is a NON-ATOMIC get-then-set fallback kept so existing
    /// third-party impls keep compiling (issue #311); real backends should override
    /// with a genuinely atomic implementation.
    fn compare_and_set(
        &self,
        flow_id: &str,
        key: &str,
        expected: Option<&Value>,
        new: Value,
    ) -> Result<CasOutcome> {
        let current = self.get(flow_id, key)?;
        if current.as_ref() == expected {
            self.set(flow_id, key, new)?;
            Ok(CasOutcome::Applied)
        } else {
            Ok(CasOutcome::Conflict(current))
        }
    }
}

/// Embedder hook for supplying a custom [`FlowStore`] per imposter (issue #312), e.g.
/// custom persistence or a test fake, without forking `rift-mock-core`. Register one on the
/// manager with
/// [`ImposterManager::with_flow_store_provider`](crate::imposter::ImposterManager::with_flow_store_provider).
/// It is consulted when an imposter's flow store is constructed, before the built-in
/// `_rift.flowState` selection.
pub trait FlowStoreProvider: Send + Sync {
    /// Return a store for this imposter, or `None` to defer to the built-ins.
    fn provide(&self, config: &crate::imposter::ImposterConfig) -> Option<Arc<dyn FlowStore>>;
}

/// A backend selectable by name through `_rift.flowState.backend` (issue #853).
///
/// This is how a store lives outside `rift-mock-core` while still being chosen by config: the
/// `"redis"` backend ships in the separate `rift-store-redis` crate and registers here, so the
/// core engine carries no redis dependency. Distinct from [`FlowStoreProvider`] in two ways that
/// matter:
///
/// - **It has an error channel.** `provide` returns `Option`, so a provider can only *decline* —
///   a misconfigured backend would silently fall through to a built-in. A factory returns
///   `Result`, so a bad URL or missing config block fails imposter creation loudly (issue #325).
/// - **It is selected by the config, not imposed on it.** A provider overrides any
///   `_rift.flowState`; a factory is only consulted when the config names it.
pub trait FlowStoreBackendFactory: Send + Sync {
    /// The `_rift.flowState.backend` string this factory serves, e.g. `"redis"`.
    fn name(&self) -> &'static str;

    /// Build a store for this imposter's `flowState` block. An `Err` fails imposter creation
    /// (fail-loud, #325) — never a silent downgrade to [`NoOpFlowStore`].
    fn build(&self, config: &crate::imposter::RiftFlowStateConfig) -> Result<Arc<dyn FlowStore>>;
}

/// The named flow-store backends a build can serve, beyond the always-present `"inmemory"`
/// (issue #853). Register with
/// [`ImposterManager::with_flow_store_backends`](crate::imposter::ImposterManager::with_flow_store_backends).
///
/// Empty by default: `rift-mock-core` alone serves only `"inmemory"`, and any other name fails
/// construction with an error listing what *is* available. The `rift` binary and the C-ABI
/// register `"redis"` (from `rift-store-redis`) when built with the `redis-backend` feature, so
/// shipped artifacts behave exactly as before.
#[derive(Clone, Default)]
pub struct FlowStoreBackends {
    factories: Vec<Arc<dyn FlowStoreBackendFactory>>,
}

impl FlowStoreBackends {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `factory` under its own [`FlowStoreBackendFactory::name`].
    ///
    /// Lookup scans in registration order, so the **first** registration of a name wins and a
    /// second one is unreachable rather than an override. That is a caller mistake with a
    /// well-defined outcome, so it warns rather than panicking — a builder in a library has no
    /// business aborting the host process over it, but it should not be silent either.
    ///
    /// Built-in names also take precedence over the registry: both selection sites match
    /// `"inmemory"` (and the `test-backend` `"failing"` store) before consulting it, so registering
    /// a factory under one of those names has no effect.
    #[must_use]
    pub fn with(mut self, factory: Arc<dyn FlowStoreBackendFactory>) -> Self {
        if self.get(factory.name()).is_some() {
            tracing::warn!(
                backend = factory.name(),
                "flow-state backend registered twice; the first registration wins and this one is unreachable"
            );
        }
        self.factories.push(factory);
        self
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn FlowStoreBackendFactory>> {
        self.factories.iter().find(|f| f.name() == name)
    }

    /// Registered backend names, for the fail-loud "available:" list.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.factories.iter().map(|f| f.name()).collect()
    }
}

impl std::fmt::Debug for FlowStoreBackends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowStoreBackends")
            .field("backends", &self.names())
            .finish()
    }
}

/// The fail-loud error for a `flowState.backend` this build cannot serve (issues #325/#377/#853).
///
/// Names the requested backend *and* everything selectable, so an operator whose binary was built
/// without a feature learns what to rebuild with — instead of a silent `NoOpFlowStore` downgrade
/// that only shows up later as state that never persists. The `test-backend` `"failing"` store is
/// deliberately not advertised: it is a test fixture, not an operator choice.
pub(crate) fn unknown_backend_error(backend: &str, backends: &FlowStoreBackends) -> anyhow::Error {
    let mut available = vec!["inmemory"];
    for name in backends.names() {
        // A name can legitimately appear twice in the registry (first registration wins), but
        // showing an operator `available: "inmemory", "redis", "redis"` would just look broken.
        if !available.contains(&name) {
            available.push(name);
        }
    }
    let list = available
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // Only reached when `backend` resolved to no factory, so an unregistered redis needs no
    // second lookup to confirm it.
    let hint = if backend == "redis" {
        " — this build has no redis backend registered (rebuild with --features redis-backend)"
    } else {
        ""
    };
    anyhow!(
        "flowState.backend is \"{backend}\" but no such backend is registered (available: {list}){hint}"
    )
}

/// No-op flow store that does nothing
///
/// This is used when flow_state is not configured but scripts are enabled.
/// Scripts that attempt to use flow state operations will get empty/default values.
/// Note: This is intentionally stateless - it's meant for scripts that don't rely on state.
#[derive(Debug)]
pub struct NoOpFlowStore;

impl FlowStore for NoOpFlowStore {
    fn get(&self, _flow_id: &str, _key: &str) -> Result<Option<Value>> {
        Ok(None)
    }

    fn set(&self, _flow_id: &str, _key: &str, _value: Value) -> Result<()> {
        Ok(())
    }

    fn exists(&self, _flow_id: &str, _key: &str) -> Result<bool> {
        Ok(false)
    }

    fn delete(&self, _flow_id: &str, _key: &str) -> Result<()> {
        Ok(())
    }

    fn increment(&self, _flow_id: &str, _key: &str) -> Result<i64> {
        // Always return 1 for no-op store since we can't track state
        // Scripts using flow_store.increment() with NoOpFlowStore will always get 1
        tracing::warn!(
            "NoOpFlowStore: increment called but no state tracking available. Configure flow_state for stateful scripts."
        );
        Ok(1)
    }

    fn set_ttl(&self, _flow_id: &str, _ttl_seconds: i64) -> Result<()> {
        Ok(())
    }

    fn set_key_ttl(&self, _flow_id: &str, _key: &str, _ttl_seconds: i64) -> Result<bool> {
        // No state is tracked, so no key can exist to expire.
        Ok(false)
    }

    fn clear_flow(&self, _flow_id: &str) -> Result<()> {
        Ok(())
    }
}

/// Create a server-level FlowStore from the `flowState` block of a proxy config file.
///
/// `"inmemory"` is built in; every other name is resolved through `backends` (issue #853), so a
/// backend living outside this crate — `"redis"`, from `rift-store-redis` — is selectable without
/// `rift-mock-core` depending on it. An unregistered name is an error listing what is available,
/// never a silent downgrade to [`NoOpFlowStore`] (issues #325/#377).
/// Reject a non-positive flow-state TTL at construction (issue #530, extended to the server-level
/// path by #860).
///
/// A non-positive TTL fails late and differently per backend — in-memory expires every write
/// immediately, Redis errors on the first `SETEX` — so a static config error would otherwise
/// surface as a runtime mystery. Shared by both the per-imposter and server-level factories so the
/// two paths cannot drift on the rule or the wording.
pub(crate) fn validate_ttl_seconds(ttl_seconds: i64) -> Result<()> {
    if ttl_seconds < 1 {
        anyhow::bail!(
            "flowState.ttlSeconds must be >= 1 (got {ttl_seconds}); a non-positive TTL would \
             expire every write immediately"
        );
    }
    Ok(())
}

pub fn create_flow_store(
    config: &crate::config::FlowStateConfig,
    backends: &FlowStoreBackends,
) -> Result<Arc<dyn FlowStore>> {
    // Before the backend match, not after: the per-imposter path validates the TTL ahead of its
    // own dispatch, so `{backend: "nope", ttlSeconds: 0}` must report the TTL error on both paths
    // rather than whichever error the dispatch happens to reach first.
    validate_ttl_seconds(config.ttl_seconds)?;
    match config.backend.as_str() {
        "inmemory" => {
            use crate::backends::InMemoryFlowStore;
            tracing::info!("Using InMemory FlowStore (ttl={}s)", config.ttl_seconds);
            Ok(Arc::new(InMemoryFlowStore::new(config.ttl_seconds as u64)))
        }
        other => match backends.get(other) {
            Some(factory) => factory.build(&server_flow_state_config(config)),
            None => Err(unknown_backend_error(other, backends)),
        },
    }
}

/// Adapt the server-level `flowState` block to the per-imposter shape a
/// [`FlowStoreBackendFactory`] takes, so both config surfaces reach one factory implementation
/// rather than each backend having to understand two config types (issue #853). `flow_id_source`
/// and `extra` have no server-level equivalent.
fn server_flow_state_config(
    config: &crate::config::FlowStateConfig,
) -> crate::imposter::RiftFlowStateConfig {
    crate::imposter::RiftFlowStateConfig {
        backend: config.backend.clone(),
        ttl_seconds: config.ttl_seconds,
        redis: config
            .redis
            .as_ref()
            .map(|r| crate::imposter::RiftRedisConfig {
                url: r.url.clone(),
                pool_size: r.pool_size,
                key_prefix: r.key_prefix.clone(),
            }),
        flow_id_source: None,
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ============================================
    // Tests for NoOpFlowStore
    // ============================================

    #[test]
    fn test_noop_flow_store_get_returns_none() {
        let store = NoOpFlowStore;
        assert!(store.get("any-flow", "any-key").unwrap().is_none());
    }

    #[test]
    fn test_noop_flow_store_set_succeeds() {
        let store = NoOpFlowStore;
        let result = store.set("flow-1", "key", json!({"data": "value"}));
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_flow_store_exists_returns_false() {
        let store = NoOpFlowStore;
        // Even after "setting" a value, exists returns false
        let _ = store.set("flow-1", "key", json!(42));
        assert!(!store.exists("flow-1", "key").unwrap());
    }

    #[test]
    fn test_noop_flow_store_delete_succeeds() {
        let store = NoOpFlowStore;
        let result = store.delete("flow-1", "key");
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_flow_store_increment_returns_one() {
        let store = NoOpFlowStore;
        // NoOpFlowStore always returns 1 for increment since it can't track state
        assert_eq!(store.increment("flow-1", "counter").unwrap(), 1);
        assert_eq!(store.increment("flow-1", "counter").unwrap(), 1);
        assert_eq!(store.increment("flow-2", "other").unwrap(), 1);
    }

    #[test]
    fn test_noop_flow_store_set_ttl_succeeds() {
        let store = NoOpFlowStore;
        assert!(store.set_ttl("flow-1", 3600).is_ok());
        assert!(store.set_ttl("flow-1", 0).is_ok());
        assert!(store.set_ttl("flow-1", -1).is_ok());
    }

    // Issue #530: NoOp has no state, so per-key ttl reports "absent" (false) and clear is a no-op.
    #[test]
    fn test_noop_flow_store_set_key_ttl_and_clear() {
        let store = NoOpFlowStore;
        assert!(!store.set_key_ttl("flow-1", "k", 60).unwrap());
        assert!(!store.set_key_ttl("flow-1", "k", 0).unwrap());
        assert!(store.clear_flow("flow-1").is_ok());
    }

    // Issue #530: a third-party store that does NOT override the new methods must keep compiling and
    // fail loud (never silently claim success) — the trait defaults enforce that.
    #[test]
    fn test_default_set_key_ttl_and_clear_are_fail_loud() {
        use std::sync::Mutex;
        struct Bare(Mutex<()>);
        impl FlowStore for Bare {
            fn get(&self, _: &str, _: &str) -> Result<Option<Value>> {
                let _g = self.0.lock();
                Ok(None)
            }
            fn set(&self, _: &str, _: &str, _: Value) -> Result<()> {
                Ok(())
            }
            fn exists(&self, _: &str, _: &str) -> Result<bool> {
                Ok(false)
            }
            fn delete(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            fn increment(&self, _: &str, _: &str) -> Result<i64> {
                Ok(1)
            }
            fn set_ttl(&self, _: &str, _: i64) -> Result<()> {
                Ok(())
            }
        }
        let store = Bare(Mutex::new(()));
        assert!(store.set_key_ttl("f", "k", 60).is_err());
        assert!(store.clear_flow("f").is_err());
    }

    #[test]
    fn test_noop_flow_store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoOpFlowStore>();
    }

    #[test]
    fn test_noop_flow_store_debug() {
        let store = NoOpFlowStore;
        let debug_str = format!("{store:?}");
        assert!(debug_str.contains("NoOpFlowStore"));
    }

    // ============================================
    // Tests for create_flow_store factory
    // ============================================

    #[test]
    fn test_create_flow_store_inmemory() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "inmemory".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let store = create_flow_store(&config, &FlowStoreBackends::new());
        assert!(store.is_ok());
    }

    #[test]
    fn test_create_flow_store_inmemory_custom_ttl() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "inmemory".to_string(),
            ttl_seconds: 7200,
            redis: None,
        };
        let store = create_flow_store(&config, &FlowStoreBackends::new());
        assert!(store.is_ok());
    }

    /// Issue #860: the server-level path had no TTL guard, so `ttlSeconds: 0` was accepted and
    /// then misbehaved late and backend-dependently — the exact failure #530 removed from the
    /// per-imposter path.
    #[test]
    fn server_level_rejects_a_non_positive_ttl() {
        use crate::config::FlowStateConfig;
        for ttl in [0, -1, -3600] {
            let config = FlowStateConfig {
                backend: "inmemory".to_string(),
                ttl_seconds: ttl,
                redis: None,
            };
            let err = build_error(
                create_flow_store(&config, &FlowStoreBackends::new()),
                &format!("ttlSeconds {ttl} must be rejected, not accepted and expired instantly"),
            );
            assert!(
                err.contains("flowState.ttlSeconds must be >= 1"),
                "ttl={ttl} gave: {err}"
            );
        }
    }

    #[test]
    fn ttl_one_is_the_boundary_and_is_accepted() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "inmemory".to_string(),
            ttl_seconds: 1,
            redis: None,
        };
        assert!(create_flow_store(&config, &FlowStoreBackends::new()).is_ok());
    }

    /// The guard runs *before* the backend match, matching the per-imposter path's ordering. If it
    /// ran after, `{backend: "nope", ttlSeconds: 0}` would report an unknown-backend error on one
    /// path and a TTL error on the other for the same config.
    #[test]
    fn a_bad_ttl_is_reported_before_an_unknown_backend() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "definitely-not-a-backend".to_string(),
            ttl_seconds: 0,
            redis: None,
        };
        let err = build_error(
            create_flow_store(&config, &FlowStoreBackends::new()),
            "a config wrong in two ways must still fail",
        );
        assert!(
            err.contains("flowState.ttlSeconds must be >= 1"),
            "TTL must be reported first, got: {err}"
        );
    }

    /// Both entry points share one guard, so they cannot drift on the rule or the wording.
    #[test]
    fn both_paths_report_the_same_ttl_error() {
        assert!(
            validate_ttl_seconds(0)
                .unwrap_err()
                .to_string()
                .contains("flowState.ttlSeconds must be >= 1")
        );
        assert!(validate_ttl_seconds(1).is_ok());
    }

    /// `Arc<dyn FlowStore>` is not `Debug`, so `expect_err` is unavailable — unwrap the error
    /// message explicitly instead.
    fn build_error(result: Result<Arc<dyn FlowStore>>, what: &str) -> String {
        match result {
            Ok(_) => panic!("{what}"),
            Err(e) => e.to_string(),
        }
    }

    /// A fake out-of-crate backend: proves the registry seam works with NO redis dependency in
    /// `rift-mock-core`, which is the whole point of issue #853.
    struct FakeBackend;

    impl FlowStoreBackendFactory for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn build(
            &self,
            _config: &crate::imposter::RiftFlowStateConfig,
        ) -> Result<Arc<dyn FlowStore>> {
            Ok(Arc::new(NoOpFlowStore))
        }
    }

    /// A backend whose construction fails — the error channel `FlowStoreProvider` lacks (#853).
    struct BrokenBackend;

    impl FlowStoreBackendFactory for BrokenBackend {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn build(
            &self,
            _config: &crate::imposter::RiftFlowStateConfig,
        ) -> Result<Arc<dyn FlowStore>> {
            Err(anyhow!("cannot reach the thing"))
        }
    }

    // Issue #853 AC3: an explicitly-named backend that nothing registered must fail with an error
    // naming it AND listing what IS selectable — never a silent NoOp downgrade (#325/#377). This
    // covers "redis" specifically, since after the extraction core registers nothing itself.
    #[test]
    fn create_flow_store_unregistered_backend_names_the_available_ones() {
        use crate::config::FlowStateConfig;
        for backend in ["redis", "unknown"] {
            let config = FlowStateConfig {
                backend: backend.to_string(),
                ttl_seconds: 300,
                redis: None,
            };
            let err = build_error(
                create_flow_store(&config, &FlowStoreBackends::new()),
                "an unregistered backend must fail, not downgrade to NoOp",
            );
            assert!(
                err.contains(backend) && err.contains("no such backend is registered"),
                "the error must name the requested backend, got: {err}"
            );
            assert!(
                err.contains("available: \"inmemory\""),
                "the error must list what IS selectable, got: {err}"
            );
        }
    }

    // Issue #853: the redis case earns a rebuild hint, because a missing feature (not a typo) is
    // overwhelmingly the reason it is absent.
    #[test]
    fn create_flow_store_unregistered_redis_hints_at_the_feature() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "redis".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let err = build_error(
            create_flow_store(&config, &FlowStoreBackends::new()),
            "redis is not registered in core",
        );
        assert!(
            err.contains("--features redis-backend"),
            "an absent redis backend must say how to get one, got: {err}"
        );
    }

    // Issue #853: a registered factory is consulted for its own name, and its registered name
    // shows up in the fail-loud list for other names.
    #[test]
    fn create_flow_store_consults_a_registered_backend() {
        use crate::config::FlowStateConfig;
        let backends = FlowStoreBackends::new().with(Arc::new(FakeBackend));
        let config = FlowStateConfig {
            backend: "fake".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        assert!(
            create_flow_store(&config, &backends).is_ok(),
            "a registered backend must be built through its factory"
        );

        let missing = FlowStateConfig {
            backend: "nope".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let err = build_error(create_flow_store(&missing, &backends), "unregistered");
        assert!(
            err.contains("\"inmemory\", \"fake\""),
            "the available list must include registered backends, got: {err}"
        );
    }

    // Issue #853: a factory's build error propagates — this is the error channel that made a
    // factory the right seam instead of `FlowStoreProvider` (whose `provide` returns Option).
    #[test]
    fn create_flow_store_propagates_a_factory_build_failure() {
        use crate::config::FlowStateConfig;
        let backends = FlowStoreBackends::new().with(Arc::new(BrokenBackend));
        let config = FlowStateConfig {
            backend: "broken".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let err = build_error(
            create_flow_store(&config, &backends),
            "a failing factory must fail the build",
        );
        assert!(
            err.contains("cannot reach the thing"),
            "the factory's own error must survive, got: {err}"
        );
    }

    // Issue #853: registration order is the contract — the FIRST factory for a name wins, so a
    // duplicate cannot silently displace a backend that is already installed.
    #[test]
    fn the_first_registration_of_a_name_wins() {
        struct Named(&'static str, &'static str);
        impl FlowStoreBackendFactory for Named {
            fn name(&self) -> &'static str {
                self.0
            }
            fn build(
                &self,
                _config: &crate::imposter::RiftFlowStateConfig,
            ) -> Result<Arc<dyn FlowStore>> {
                Err(anyhow!("built by {}", self.1))
            }
        }

        let backends = FlowStoreBackends::new()
            .with(Arc::new(Named("dup", "first")))
            .with(Arc::new(Named("dup", "second")));
        assert_eq!(backends.names(), vec!["dup", "dup"]);

        let config = crate::config::FlowStateConfig {
            backend: "dup".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let err = build_error(create_flow_store(&config, &backends), "always errors");
        assert!(
            err.contains("built by first"),
            "the first registration must be the one consulted, got: {err}"
        );
    }

    // Issue #853: built-ins are matched before the registry is consulted, so a factory claiming a
    // built-in name is inert. Pinned so a future reorder of the match cannot silently start letting
    // an embedder replace `"inmemory"`.
    #[test]
    fn a_factory_cannot_shadow_the_builtin_inmemory_backend() {
        struct Hijack;
        impl FlowStoreBackendFactory for Hijack {
            fn name(&self) -> &'static str {
                "inmemory"
            }
            fn build(
                &self,
                _config: &crate::imposter::RiftFlowStateConfig,
            ) -> Result<Arc<dyn FlowStore>> {
                Err(anyhow!("the hijacking factory must never be consulted"))
            }
        }

        let backends = FlowStoreBackends::new().with(Arc::new(Hijack));
        let config = crate::config::FlowStateConfig {
            backend: "inmemory".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        assert!(
            create_flow_store(&config, &backends).is_ok(),
            "the built-in inmemory backend must win over a registered factory of the same name"
        );
    }

    // Issue #853: the server-level `redis` block must reach the factory — otherwise a config-file
    // deployment would hand the backend an empty config and fail for the wrong reason.
    #[test]
    fn server_level_redis_block_reaches_the_factory() {
        use crate::config::{FlowStateConfig, RedisConfig};
        let config = FlowStateConfig {
            backend: "redis".to_string(),
            ttl_seconds: 900,
            redis: Some(RedisConfig {
                url: "redis://example:6379".to_string(),
                pool_size: 7,
                key_prefix: "pfx:".to_string(),
            }),
        };
        let adapted = server_flow_state_config(&config);
        assert_eq!(adapted.backend, "redis");
        assert_eq!(adapted.ttl_seconds, 900);
        let redis = adapted.redis.expect("the redis block must carry over");
        assert_eq!(redis.url, "redis://example:6379");
        assert_eq!(redis.pool_size, 7);
        assert_eq!(redis.key_prefix, "pfx:");
    }

    // ============================================
    // Tests for FlowStore trait object behavior
    // ============================================

    #[test]
    fn test_flow_store_as_trait_object() {
        let store: Arc<dyn FlowStore> = Arc::new(NoOpFlowStore);

        // Should be able to call all trait methods through the trait object
        assert!(store.get("flow", "key").unwrap().is_none());
        assert!(store.set("flow", "key", json!(1)).is_ok());
        assert!(!store.exists("flow", "key").unwrap());
        assert!(store.delete("flow", "key").is_ok());
        assert_eq!(store.increment("flow", "counter").unwrap(), 1);
        assert!(store.set_ttl("flow", 100).is_ok());
    }

    #[test]
    fn test_flow_store_clone_arc() {
        let store: Arc<dyn FlowStore> = Arc::new(NoOpFlowStore);
        let store2 = Arc::clone(&store);

        // Both references should work
        assert!(store.get("flow", "key").unwrap().is_none());
        assert!(store2.get("flow", "key").unwrap().is_none());
    }

    // ============================================
    // Tests with various JSON value types
    // ============================================

    #[test]
    fn test_noop_flow_store_with_string_value() {
        let store = NoOpFlowStore;
        assert!(store.set("flow", "key", json!("hello")).is_ok());
    }

    #[test]
    fn test_noop_flow_store_with_number_value() {
        let store = NoOpFlowStore;
        assert!(store.set("flow", "key", json!(42)).is_ok());
        assert!(store.set("flow", "key", json!(1.5)).is_ok());
        assert!(store.set("flow", "key", json!(-100)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_with_boolean_value() {
        let store = NoOpFlowStore;
        assert!(store.set("flow", "key", json!(true)).is_ok());
        assert!(store.set("flow", "key", json!(false)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_with_null_value() {
        let store = NoOpFlowStore;
        assert!(store.set("flow", "key", json!(null)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_with_array_value() {
        let store = NoOpFlowStore;
        assert!(store.set("flow", "key", json!([1, 2, 3])).is_ok());
        assert!(store.set("flow", "key", json!(["a", "b", "c"])).is_ok());
    }

    #[test]
    fn test_noop_flow_store_with_object_value() {
        let store = NoOpFlowStore;
        assert!(
            store
                .set("flow", "key", json!({"nested": {"deep": "value"}}))
                .is_ok()
        );
    }

    // ============================================
    // Tests for edge cases
    // ============================================

    #[test]
    fn test_noop_flow_store_empty_flow_id() {
        let store = NoOpFlowStore;
        assert!(store.get("", "key").unwrap().is_none());
        assert!(store.set("", "key", json!(1)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_empty_key() {
        let store = NoOpFlowStore;
        assert!(store.get("flow", "").unwrap().is_none());
        assert!(store.set("flow", "", json!(1)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_special_characters() {
        let store = NoOpFlowStore;
        let flow_id = "flow:with:colons";
        let key = "key/with/slashes";
        assert!(store.get(flow_id, key).unwrap().is_none());
        assert!(store.set(flow_id, key, json!(1)).is_ok());
    }

    #[test]
    fn test_noop_flow_store_unicode() {
        let store = NoOpFlowStore;
        let flow_id = "流程-123";
        let key = "键值";
        assert!(store.get(flow_id, key).unwrap().is_none());
        assert!(store.set(flow_id, key, json!("データ")).is_ok());
    }
}

// The last script flow-store op's error for the current thread, or `None` if the last op
// succeeded. Set/cleared by `log_flow_err` on every op so a script can observe a backend failure
// via `flow_store.last_error()` instead of only getting a silent fallback value (issue #322).
thread_local! {
    static LAST_FLOW_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Take (read and clear) the last script flow-store error for the current thread. Backs the
/// per-engine `flow_store.last_error()` accessors (issue #322).
pub fn take_last_flow_error() -> Option<String> {
    LAST_FLOW_ERROR.with(|e| e.borrow_mut().take())
}

/// Reset the last flow-store error at the start of a script execution, so `last_error()` reflects
/// only ops from THIS execution — never a stale value left on a reused (pooled) worker thread
/// (issue #322).
pub fn clear_last_flow_error() {
    LAST_FLOW_ERROR.with(|e| *e.borrow_mut() = None);
}

/// Route a script flow-store op result through the shared error seam: on failure, log it and record
/// it so `flow_store.last_error()` can surface it (issue #322), returning the error MESSAGE so the
/// caller can raise it as a script error; on success, clear the slot so `last_error()` reflects
/// only the most recent op. [`log_flow_err`] is the fallback-returning wrapper built on this.
pub fn flow_result<T>(op: &str, result: Result<T>) -> std::result::Result<T, String> {
    match result {
        Ok(value) => {
            LAST_FLOW_ERROR.with(|e| *e.borrow_mut() = None);
            Ok(value)
        }
        Err(e) => {
            let msg = format!("{op}: {e:#}");
            tracing::warn!("script flow_store.{op} failed: {e:#}");
            LAST_FLOW_ERROR.with(|slot| *slot.borrow_mut() = Some(msg.clone()));
            Err(msg)
        }
    }
}

/// Route a script flow-store op result through the shared error seam, returning the fallback on
/// failure (recording it for `flow_store.last_error()`, issue #322).
pub fn log_flow_err<T>(op: &str, fallback: T, result: Result<T>) -> T {
    flow_result(op, result).unwrap_or(fallback)
}

/// A deliberately failing flow store (feature `test-backend`): every operation annotates
/// the op via `decorate::annotate`, then fails with a `BackendUnavailable` source —
/// selected with `_rift.flowState.backend = "failing"` to exercise backend-outage paths
/// (issue #318) without a real unreachable backend.
#[cfg(feature = "test-backend")]
#[derive(Debug)]
pub struct FailingFlowStore;

#[cfg(feature = "test-backend")]
impl FailingFlowStore {
    fn fail<T>(&self, op: &'static str, flow_id: &str, key: &str) -> Result<T> {
        crate::extensions::decorate::annotate(op, format!("{flow_id}/{key}"));
        Err(anyhow::Error::new(
            crate::extensions::decorate::BackendUnavailable {
                feature: "flowState",
                detail: format!("failing test backend: {op} {flow_id}/{key}"),
            },
        ))
    }
}

#[cfg(feature = "test-backend")]
impl FlowStore for FailingFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
        self.fail("flowStore.get", flow_id, key)
    }
    fn set(&self, flow_id: &str, key: &str, _value: Value) -> Result<()> {
        self.fail("flowStore.set", flow_id, key)
    }
    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        self.fail("flowStore.exists", flow_id, key)
    }
    fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
        self.fail("flowStore.delete", flow_id, key)
    }
    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        self.fail("flowStore.increment", flow_id, key)
    }
    fn set_ttl(&self, flow_id: &str, _ttl_seconds: i64) -> Result<()> {
        self.fail("flowStore.setTtl", flow_id, "")
    }
    fn set_key_ttl(&self, flow_id: &str, key: &str, _ttl_seconds: i64) -> Result<bool> {
        self.fail("flowStore.setKeyTtl", flow_id, key)
    }
    fn clear_flow(&self, flow_id: &str) -> Result<()> {
        self.fail("flowStore.clearFlow", flow_id, "")
    }
}

#[cfg(test)]
mod last_flow_error_tests {
    use super::*;

    // AC1 (issue #322): log_flow_err records a failure so last_error can surface it, and a
    // subsequent success clears it — so last_error reflects only the most recent op.
    #[test]
    fn log_flow_err_records_error_and_clears_on_ok() {
        let _ = take_last_flow_error(); // start clean on this thread
        let out = log_flow_err("get", None::<i32>, Err(anyhow::anyhow!("redis down")));
        assert_eq!(out, None);
        let recorded = take_last_flow_error();
        assert!(
            recorded
                .as_deref()
                .is_some_and(|s| s.contains("get") && s.contains("redis down")),
            "a failed op must record its error, got {recorded:?}"
        );
        // take() cleared it
        assert_eq!(take_last_flow_error(), None);
        // a success clears the slot
        LAST_FLOW_ERROR.with(|e| *e.borrow_mut() = Some("stale".to_string()));
        let v = log_flow_err("set", false, Ok::<bool, anyhow::Error>(true));
        assert!(v);
        assert_eq!(
            take_last_flow_error(),
            None,
            "a successful op clears last_error"
        );
    }
}

#[cfg(test)]
mod cas_tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// A third-party-style store that does NOT override compare_and_set — proves the
    /// provided default keeps existing impls compiling and gives get-then-set semantics.
    struct MinimalStore {
        data: Mutex<std::collections::HashMap<String, Value>>,
    }

    impl MinimalStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    impl FlowStore for MinimalStore {
        fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
            Ok(self
                .data
                .lock()
                .expect("test lock")
                .get(&format!("{flow_id}:{key}"))
                .cloned())
        }
        fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()> {
            self.data
                .lock()
                .expect("test lock")
                .insert(format!("{flow_id}:{key}"), value);
            Ok(())
        }
        fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
            Ok(self.get(flow_id, key)?.is_some())
        }
        fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
            self.data
                .lock()
                .expect("test lock")
                .remove(&format!("{flow_id}:{key}"));
            Ok(())
        }
        fn increment(&self, _flow_id: &str, _key: &str) -> Result<i64> {
            Ok(1)
        }
        fn set_ttl(&self, _flow_id: &str, _ttl_seconds: i64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_cas_applies_when_expected_matches() {
        let store = MinimalStore::new();
        let outcome = store
            .compare_and_set("f", "k", None, json!("v1"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Applied));
        assert_eq!(store.get("f", "k").expect("get"), Some(json!("v1")));

        let outcome = store
            .compare_and_set("f", "k", Some(&json!("v1")), json!("v2"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Applied));
        assert_eq!(store.get("f", "k").expect("get"), Some(json!("v2")));
    }

    #[test]
    fn default_cas_conflicts_with_current_value() {
        let store = MinimalStore::new();
        store.set("f", "k", json!("actual")).expect("set");

        let outcome = store
            .compare_and_set("f", "k", Some(&json!("expected")), json!("new"))
            .expect("cas");
        match outcome {
            CasOutcome::Conflict(current) => assert_eq!(current, Some(json!("actual"))),
            CasOutcome::Applied => panic!("must conflict"),
        }
        assert_eq!(
            store.get("f", "k").expect("get"),
            Some(json!("actual")),
            "conflict must not write"
        );

        let outcome = store
            .compare_and_set("f", "k", None, json!("new"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Conflict(Some(_))));
    }

    // Issue #358 B4: the trait's default (non-atomic) increment_by must error on i64 overflow via
    // checked_add, never panic (debug) or wrap (release).
    #[test]
    fn default_increment_by_overflow_errors() {
        let store = MinimalStore::new();
        store.set("f", "k", json!(i64::MAX)).expect("set");
        assert!(
            store.increment_by("f", "k", 1).is_err(),
            "default increment_by past i64::MAX must error"
        );
    }

    #[test]
    fn default_increment_by_starts_at_zero_and_accumulates() {
        let store = MinimalStore::new();
        assert_eq!(store.increment_by("f", "k", 5).expect("incr"), 5);
        assert_eq!(store.increment_by("f", "k", 5).expect("incr"), 10);
    }
}
