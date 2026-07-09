use anyhow::{Context, Result, anyhow};
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
/// when called from Lua scripts or other synchronous contexts.
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

    /// Set TTL for all keys under a flow_id
    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()>;

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
/// custom persistence or a test fake, without forking `rift-core`. Register one on the
/// manager with
/// [`ImposterManager::with_flow_store_provider`](crate::imposter::ImposterManager::with_flow_store_provider).
/// It is consulted when an imposter's flow store is constructed, before the built-in
/// `_rift.flowState` selection.
pub trait FlowStoreProvider: Send + Sync {
    /// Return a store for this imposter, or `None` to defer to the built-ins.
    fn provide(&self, config: &crate::imposter::ImposterConfig) -> Option<Arc<dyn FlowStore>>;
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
}

/// Create a FlowStore based on configuration
///
/// # Arguments
/// * `config` - FlowStateConfig from proxy configuration
///
/// # Returns
/// * `Arc<dyn FlowStore>` - Backend-appropriate flow store
///
/// # Example
/// ```ignore
/// use rift_http_proxy::config::FlowStateConfig;
/// use rift_http_proxy::flow_state::create_flow_store;
///
/// let config = FlowStateConfig::default(); // inmemory
/// let store = create_flow_store(&config).await?;
/// ```
pub fn create_flow_store(config: &crate::config::FlowStateConfig) -> Result<Arc<dyn FlowStore>> {
    match config.backend.as_str() {
        "inmemory" => {
            use crate::backends::InMemoryFlowStore;
            tracing::info!("Using InMemory FlowStore (ttl={}s)", config.ttl_seconds);
            Ok(Arc::new(InMemoryFlowStore::new(config.ttl_seconds as u64)))
        }
        "redis" => {
            let redis_config = config
                .redis
                .as_ref()
                .ok_or_else(|| anyhow!("Redis backend selected but no redis config provided"))?;

            #[cfg(feature = "redis-backend")]
            {
                use crate::backends::RedisFlowStore;

                let store = RedisFlowStore::new(
                    &redis_config.url,
                    redis_config.pool_size,
                    redis_config.key_prefix.clone(),
                    config.ttl_seconds,
                )
                .context("Failed to create Redis backend")?;

                tracing::info!(
                    "Using redis FlowStore (url={}, ttl={}s)",
                    redis_config.url,
                    config.ttl_seconds
                );

                Ok(Arc::new(store))
            }

            #[cfg(not(feature = "redis-backend"))]
            {
                Err(anyhow!(
                    "Redis backend not available. Compile with --features redis-backend"
                ))
            }
        }
        other => Err(anyhow!("Unknown backend type: {other}")),
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
        let store = create_flow_store(&config);
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
        let store = create_flow_store(&config);
        assert!(store.is_ok());
    }

    #[test]
    fn test_create_flow_store_unknown_backend() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "unknown".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let result = create_flow_store(&config);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("Unknown backend type"));
    }

    #[test]
    fn test_create_flow_store_redis_without_config() {
        use crate::config::FlowStateConfig;
        let config = FlowStateConfig {
            backend: "redis".to_string(),
            ttl_seconds: 300,
            redis: None,
        };
        let result = create_flow_store(&config);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("redis config"));
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
/// it so `flow_store.last_error()` can surface it (issue #322), returning the error MESSAGE so a
/// strict engine can raise it (issue #376); on success, clear the slot so `last_error()` reflects
/// only the most recent op. [`log_flow_err`] is the lenient wrapper built on this.
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
/// failure (recording it for `flow_store.last_error()`, issue #322). This is the lenient contract:
/// a backend outage yields a fallback value rather than a script error. The strict alternative
/// (issue #376, gated by [`strict_flow_store`]) uses [`flow_result`] directly to raise instead.
pub fn log_flow_err<T>(op: &str, fallback: T, result: Result<T>) -> T {
    flow_result(op, result).unwrap_or(fallback)
}

/// Whether script flow-store ops fail loud — raise a native script error on a backend failure
/// instead of returning a fallback value + recording `last_error()` (issue #376). Read once from
/// the `RIFT_STRICT_FLOW_STORE` env var (`1`/`true`/`yes`/`on`). Default off preserves the lenient
/// #322 contract. A global gate (like `RIFT_DISABLE_HTTP2`, #378) because scripts run on pooled
/// worker threads where a per-imposter flag can't be threaded without cross-engine plumbing.
pub fn strict_flow_store() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        strict_flow_store_from(std::env::var("RIFT_STRICT_FLOW_STORE").ok().as_deref())
    })
}

/// Pure parse of the `RIFT_STRICT_FLOW_STORE` value, split out so it can be unit-tested without the
/// process-global env-var races that a full end-to-end test would hit.
fn strict_flow_store_from(val: Option<&str>) -> bool {
    val.map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
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
}

#[cfg(test)]
mod last_flow_error_tests {
    use super::*;

    // AC5 (issue #376): RIFT_STRICT_FLOW_STORE parsing — truthy values fail loud, everything else
    // keeps the lenient #322 fallback.
    #[test]
    fn strict_flow_store_env_parsing() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(
                strict_flow_store_from(Some(on)),
                "{on:?} should enable strict flow-store mode"
            );
        }
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("2"),
        ] {
            assert!(
                !strict_flow_store_from(off),
                "{off:?} should keep the lenient fallback"
            );
        }
    }

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
