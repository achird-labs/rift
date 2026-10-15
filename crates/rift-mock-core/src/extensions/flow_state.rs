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
/// A networked implementation (supplied by an embedder) is expected to use a blocking client.
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
    /// block, so the default is `false`; a networked store overrides it.
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
    /// implementation (see `InMemoryFlowStore`).
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

    /// List every flow id this store currently holds live (non-expired) state for (issue #374).
    ///
    /// A flow's "space" is created implicitly by whatever flow id a request happened to carry —
    /// there is no explicit registration step — so today the only way to learn which spaces exist
    /// is to read the request log and infer it from `_rift.flowState`-touching requests. This is
    /// the enumeration primitive an "Active spaces" admin view needs instead.
    ///
    /// Defaulted, not required: `FlowStore` is implemented by the built-in backends *and* by any
    /// embedder through [`FlowStoreProvider`] — which exists precisely so embedders can plug their
    /// own store in. A required method would break every external implementor at compile time. The
    /// trait already establishes this pattern: `increment_by`, `set_key_ttl` and `clear_flow` all
    /// ship defaults for exactly this reason.
    ///
    /// Returns `Option`, not a bare `Vec`, for a reason that matters: `None` means "this store
    /// cannot enumerate", while `Some(vec![])` means "it can, and there are none right now".
    /// Collapsing those two into one empty-list answer would make an admin surface render "no
    /// spaces exist" as fact when the truth is "this backend cannot tell you" — a wrong-but-quiet
    /// answer that is worse than an honest "unsupported". The default is `Ok(None)`: an
    /// unenumerable store by construction, since it has no data to walk.
    fn flow_ids(&self) -> Result<Option<Vec<String>>> {
        Ok(None)
    }

    /// Count the live (non-expired) keys currently stored under `flow_id` (issue #374) — the
    /// per-space size an "Active spaces" table would show next to each id from [`Self::flow_ids`].
    ///
    /// Same reasoning as `flow_ids` on both counts: defaulted (not required) so third-party
    /// `FlowStore` impls keep compiling, and `Option`-typed so "this store cannot answer" (`None`)
    /// stays distinguishable from "the flow has zero live keys, or does not exist" (`Some(0)`) —
    /// again, collapsing the two would let an admin screen quietly lie about a backend it cannot
    /// actually inspect.
    fn entry_count(&self, flow_id: &str) -> Result<Option<usize>> {
        let _ = flow_id;
        Ok(None)
    }
}

/// Embedder hook for supplying a custom [`FlowStore`] per imposter (issue #312), e.g.
/// custom persistence or a test fake, without forking `rift-mock-core`. Register one on the
/// manager with
/// [`ImposterManager::with_flow_store_provider`](crate::imposter::ImposterManager::with_flow_store_provider).
/// It is consulted when an imposter's flow store is constructed, before the built-in in-memory
/// store. It is the only way to put a store other than the in-memory one behind an imposter.
pub trait FlowStoreProvider: Send + Sync {
    /// Return a store for this imposter, or `None` to defer to the built-in selection.
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

    fn set_key_ttl(&self, _flow_id: &str, _key: &str, _ttl_seconds: i64) -> Result<bool> {
        // No state is tracked, so no key can exist to expire.
        Ok(false)
    }

    fn clear_flow(&self, _flow_id: &str) -> Result<()> {
        Ok(())
    }
}

/// Counts every flow-store operation into `rift_flow_state_ops_total` (issue #999), by delegating
/// to an inner store.
///
/// Wrapped once around the store `Imposter::create_flow_store` returns, so the provider path,
/// `inmemory` and the NoOp fallback are all covered without touching a single call site.
///
/// **Every trait method is forwarded, including the ones with default bodies — that is a
/// correctness requirement, not tidiness.** `is_blocking` defaults to `false`, so failing to
/// forward it would silently reclassify a *blocking* backend as non-blocking and break the
/// `spawn_blocking` routing added in #985/#988/#989. The other defaults are written in terms of
/// `get`/`set`, so inheriting them would discard the inner store's atomic implementations —
/// turning an atomic networked `INCRBY` (or `InMemoryFlowStore::increment_by`) into a racy
/// get-then-set. Recording happens *after* the inner call and never alters the returned `Result`.
pub struct MeteredFlowStore {
    inner: Arc<dyn FlowStore>,
}

impl MeteredFlowStore {
    #[must_use]
    pub fn new(inner: Arc<dyn FlowStore>) -> Self {
        Self { inner }
    }
}

/// Record `op` against the outcome and hand the result straight back.
fn metered<T>(op: &str, result: Result<T>) -> Result<T> {
    crate::extensions::metrics::record_flow_state_op(op, result.is_ok());
    result
}

impl FlowStore for MeteredFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
        metered("get", self.inner.get(flow_id, key))
    }

    fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()> {
        metered("set", self.inner.set(flow_id, key, value))
    }

    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        metered("exists", self.inner.exists(flow_id, key))
    }

    fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
        metered("delete", self.inner.delete(flow_id, key))
    }

    fn is_blocking(&self) -> bool {
        self.inner.is_blocking()
    }

    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        metered("increment", self.inner.increment(flow_id, key))
    }

    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        metered("increment_by", self.inner.increment_by(flow_id, key, by))
    }

    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()> {
        metered("set_ttl", self.inner.set_ttl(flow_id, ttl_seconds))
    }

    fn set_key_ttl(&self, flow_id: &str, key: &str, ttl_seconds: i64) -> Result<bool> {
        metered(
            "set_key_ttl",
            self.inner.set_key_ttl(flow_id, key, ttl_seconds),
        )
    }

    fn clear_flow(&self, flow_id: &str) -> Result<()> {
        metered("clear_flow", self.inner.clear_flow(flow_id))
    }

    fn compare_and_set(
        &self,
        flow_id: &str,
        key: &str,
        expected: Option<&Value>,
        new: Value,
    ) -> Result<CasOutcome> {
        metered(
            "compare_and_set",
            self.inner.compare_and_set(flow_id, key, expected, new),
        )
    }

    fn flow_ids(&self) -> Result<Option<Vec<String>>> {
        metered("flow_ids", self.inner.flow_ids())
    }

    fn entry_count(&self, flow_id: &str) -> Result<Option<usize>> {
        metered("entry_count", self.inner.entry_count(flow_id))
    }
}

/// Reject a non-positive flow-state TTL at construction (issue #530).
///
/// A non-positive TTL fails late and differently per store — in-memory expires every write
/// immediately, a networked store may error on the first `SETEX` — so a static config error would
/// otherwise surface as a runtime mystery.
pub(crate) fn validate_ttl_seconds(ttl_seconds: i64) -> Result<()> {
    if ttl_seconds < 1 {
        anyhow::bail!(
            "flowState.ttlSeconds must be >= 1 (got {ttl_seconds}); a non-positive TTL would \
             expire every write immediately"
        );
    }
    Ok(())
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

    /// The guard's boundary and wording: `1` is accepted, `0` is refused with the documented text.
    #[test]
    fn ttl_guard_boundary_and_wording() {
        assert!(
            validate_ttl_seconds(0)
                .unwrap_err()
                .to_string()
                .contains("flowState.ttlSeconds must be >= 1")
        );
        assert!(validate_ttl_seconds(1).is_ok());
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

/// A deliberately failing flow store for the scripting engines' unit tests: every operation
/// annotates the op via `decorate::annotate`, then fails with a `BackendUnavailable` source, to
/// exercise backend-outage paths (issue #318) without a real unreachable backend. Integration
/// tests install an equivalent store through a `FlowStoreProvider` (issue #1463).
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailingFlowStore;

#[cfg(test)]
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

#[cfg(test)]
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
        let out = log_flow_err("get", None::<i32>, Err(anyhow::anyhow!("store down")));
        assert_eq!(out, None);
        let recorded = take_last_flow_error();
        assert!(
            recorded
                .as_deref()
                .is_some_and(|s| s.contains("get") && s.contains("store down")),
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

/// Issue #999: the metering decorator must be *transparent*. These two tests exist because the
/// dangerous failure of a delegating wrapper around a trait with default bodies is not a wrong
/// metric — it is silently changing the store's behaviour.
#[cfg(test)]
mod metered_flow_store_tests {
    use super::*;

    struct FakeInner {
        blocking: bool,
    }

    impl FlowStore for FakeInner {
        fn get(&self, _: &str, _: &str) -> Result<Option<Value>> {
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
        fn is_blocking(&self) -> bool {
            self.blocking
        }
        /// A sentinel the trait's default body could never produce: the default is defined in
        /// terms of `get`/`set`, and this store's `get` returns `None`, so the default would
        /// yield `by`. Observing 4242 proves the call actually reached the inner store.
        fn increment_by(&self, _: &str, _: &str, _by: i64) -> Result<i64> {
            Ok(4242)
        }
    }

    /// `is_blocking()` defaults to `false`. A decorator that fails to forward it silently
    /// reclassifies a blocking backend as non-blocking, which breaks the `spawn_blocking`
    /// routing added in #985/#988/#989 — a concurrency bug, not a metrics inaccuracy.
    #[test]
    fn metered_store_forwards_is_blocking_in_both_directions() {
        let blocking = MeteredFlowStore::new(Arc::new(FakeInner { blocking: true }));
        assert!(
            blocking.is_blocking(),
            "a blocking inner store must still report as blocking through the decorator"
        );

        let non_blocking = MeteredFlowStore::new(Arc::new(FakeInner { blocking: false }));
        assert!(
            !non_blocking.is_blocking(),
            "a non-blocking inner store must not be reported as blocking"
        );
    }

    /// Every defaulted method must be forwarded, not inherited. `increment_by`'s default body is
    /// written in terms of `get`/`set`, so relying on it would discard the inner store's atomic
    /// implementation — turning an atomic INCRBY into a racy get-then-set.
    #[test]
    fn metered_store_forwards_defaulted_methods_to_inner() {
        let store = MeteredFlowStore::new(Arc::new(FakeInner { blocking: false }));
        assert_eq!(
            store.increment_by("flow", "key", 7).expect("increment_by"),
            4242,
            "increment_by must reach the inner store's implementation, not the trait default"
        );
    }
}
