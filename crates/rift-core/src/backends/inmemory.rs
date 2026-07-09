use crate::extensions::flow_state::{CasOutcome, FlowStore};
use anyhow::Result;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// In-memory implementation of FlowStore
///
/// This implementation stores flow state in a HashMap with automatic TTL expiration.
/// Useful for testing, development, and single-instance deployments.
pub struct InMemoryFlowStore {
    #[allow(clippy::type_complexity)]
    data: Arc<RwLock<HashMap<String, (Value, Option<SystemTime>)>>>,
    default_ttl: Duration,
}

impl InMemoryFlowStore {
    pub fn new(default_ttl_seconds: u64) -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            default_ttl: Duration::from_secs(default_ttl_seconds),
        }
    }

    fn make_key(&self, flow_id: &str, key: &str) -> String {
        format!("flow:{flow_id}:{key}")
    }

    /// List every non-expired key currently stored under `flow_id`, for `rift script run`'s
    /// post-execution state dump (issue #360 Item 2) — the [`FlowStore`] trait itself has no
    /// enumeration method (a real backend like Redis may not make that cheap), but the CLI works
    /// with a concrete `InMemoryFlowStore` fixture, so an inherent method here is enough.
    pub fn keys_for_flow(&self, flow_id: &str) -> Vec<String> {
        let prefix = format!("flow:{flow_id}:");
        let data = self.data.read();
        data.iter()
            .filter(|(_, (_, expiry))| !self.is_expired(expiry))
            .filter_map(|(key, _)| key.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    fn is_expired(&self, expiry: &Option<SystemTime>) -> bool {
        if let Some(exp) = expiry {
            SystemTime::now() > *exp
        } else {
            false
        }
    }

    /// Opportunistically clean up a specific expired key during write operations.
    /// This is called with the lock already held to avoid separate cleanup passes.
    ///
    /// Note: `data` parameter must be a mutable reference acquired from self.data.lock()
    fn cleanup_on_write(
        data: &mut HashMap<String, (Value, Option<SystemTime>)>,
        key: &str,
        is_expired_fn: impl Fn(&Option<SystemTime>) -> bool,
    ) {
        if let Some((_, expiry)) = data.get(key)
            && is_expired_fn(expiry)
        {
            data.remove(key);
        }
    }
}

impl FlowStore for InMemoryFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
        // Use read lock for concurrent read access
        let key_str = self.make_key(flow_id, key);
        let data = self.data.read();

        match data.get(&key_str) {
            Some((value, expiry)) if !self.is_expired(expiry) => Ok(Some(value.clone())),
            _ => Ok(None),
        }
    }

    fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()> {
        let key_str = self.make_key(flow_id, key);
        let expiry = SystemTime::now() + self.default_ttl;
        let mut data = self.data.write();

        // Opportunistically clean up this specific key if expired
        Self::cleanup_on_write(&mut data, &key_str, |exp| self.is_expired(exp));

        data.insert(key_str, (value, Some(expiry)));
        Ok(())
    }

    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        // Use read lock for concurrent read access
        let key_str = self.make_key(flow_id, key);
        let data = self.data.read();

        match data.get(&key_str) {
            Some((_, expiry)) if !self.is_expired(expiry) => Ok(true),
            _ => Ok(false),
        }
    }

    fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
        let key_str = self.make_key(flow_id, key);
        let mut data = self.data.write();
        data.remove(&key_str);
        Ok(())
    }

    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        self.increment_by(flow_id, key, 1)
    }

    /// Atomic under the single write lock (issue #358), like `increment`: the read-modify-write
    /// happens with the lock held, so no interleaving window with a concurrent increment/set.
    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        let key_str = self.make_key(flow_id, key);
        let expiry = SystemTime::now() + self.default_ttl;
        let mut data = self.data.write();

        // Opportunistically clean up this specific key if expired
        Self::cleanup_on_write(&mut data, &key_str, |exp| self.is_expired(exp));

        let current = match data.get(&key_str) {
            Some((Value::Number(n), _)) if n.is_i64() => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        // `checked_add` so an overflow near i64::MAX errors (fail-loud) instead of panicking in
        // debug / wrapping in release — matching Redis's INCRBY, which also errors on overflow.
        let new_value = current.checked_add(by).ok_or_else(|| {
            anyhow::anyhow!("increment_by overflow: {current} + {by} exceeds i64 range")
        })?;

        data.insert(key_str, (Value::Number(new_value.into()), Some(expiry)));
        Ok(new_value)
    }

    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()> {
        let prefix = format!("flow:{flow_id}:");
        let new_expiry =
            SystemTime::now() + Duration::from_secs(u64::try_from(ttl_seconds).unwrap_or(0));
        let mut data = self.data.write();

        for (key, (_, expiry)) in data.iter_mut() {
            if key.starts_with(&prefix) {
                *expiry = Some(new_expiry);
            }
        }

        Ok(())
    }

    /// Atomic under the single write lock (issue #311): compare and write happen with no
    /// interleaving window, unlike the trait's get-then-set default.
    fn compare_and_set(
        &self,
        flow_id: &str,
        key: &str,
        expected: Option<&Value>,
        new: Value,
    ) -> Result<CasOutcome> {
        let key_str = self.make_key(flow_id, key);
        let mut data = self.data.write();
        Self::cleanup_on_write(&mut data, &key_str, |exp| self.is_expired(exp));

        let current = data.get(&key_str).map(|(value, _)| value);
        if current == expected {
            let expiry = SystemTime::now() + self.default_ttl;
            data.insert(key_str, (new, Some(expiry)));
            Ok(CasOutcome::Applied)
        } else {
            Ok(CasOutcome::Conflict(current.cloned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #360 Item 2: `rift script run` dumps the flow's keys after execution.
    #[test]
    fn keys_for_flow_lists_only_that_flows_non_expired_keys() {
        let store = InMemoryFlowStore::new(300);
        store.set("flow1", "attempts", json!(2)).unwrap();
        store.set("flow1", "last_status", json!("ok")).unwrap();
        store.set("other-flow", "unrelated", json!(true)).unwrap();

        let mut keys = store.keys_for_flow("flow1");
        keys.sort();
        assert_eq!(
            keys,
            vec!["attempts".to_string(), "last_status".to_string()]
        );

        assert!(store.keys_for_flow("no-such-flow").is_empty());
    }

    #[test]
    fn keys_for_flow_excludes_expired_keys() {
        let store = InMemoryFlowStore::new(1);
        store.set("flow1", "key1", json!("value1")).unwrap();
        assert_eq!(store.keys_for_flow("flow1"), vec!["key1".to_string()]);
        std::thread::sleep(Duration::from_secs(2));
        assert!(store.keys_for_flow("flow1").is_empty());
    }

    #[test]
    fn test_inmemory_get_set() {
        let store = InMemoryFlowStore::new(300);

        store.set("flow1", "key1", json!("value1")).unwrap();
        let value = store.get("flow1", "key1").unwrap();
        assert_eq!(value, Some(json!("value1")));
    }

    #[test]
    fn test_inmemory_exists() {
        let store = InMemoryFlowStore::new(300);

        store.set("flow1", "key1", json!("value1")).unwrap();
        assert!(store.exists("flow1", "key1").unwrap());
        assert!(!store.exists("flow1", "key2").unwrap());
    }

    #[test]
    fn test_inmemory_delete() {
        let store = InMemoryFlowStore::new(300);

        store.set("flow1", "key1", json!("value1")).unwrap();
        store.delete("flow1", "key1").unwrap();
        let value = store.get("flow1", "key1").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn test_inmemory_increment() {
        let store = InMemoryFlowStore::new(300);

        let v1 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v1, 1);

        let v2 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v2, 2);

        let v3 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v3, 3);
    }

    #[test]
    fn test_inmemory_ttl() {
        let store = InMemoryFlowStore::new(1); // 1 second TTL

        store.set("flow1", "key1", json!("value1")).unwrap();

        // Should exist immediately
        assert!(store.exists("flow1", "key1").unwrap());

        // Wait for expiry
        std::thread::sleep(Duration::from_secs(2));

        // Should be expired
        assert!(!store.exists("flow1", "key1").unwrap());
    }

    #[test]
    fn test_concurrent_set_get_same_flow() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryFlowStore::new(300));
        let num_threads = 10;
        let iterations_per_thread = 100;

        // Spawn multiple threads that rapidly write and read from the same flow_id
        let handles: Vec<_> = (0..num_threads)
            .map(|thread_id| {
                let store_clone = Arc::clone(&store);
                thread::spawn(move || {
                    for i in 0..iterations_per_thread {
                        let value = json!(format!("thread_{}_value_{}", thread_id, i));

                        // Write
                        store_clone
                            .set("shared_flow", &format!("key_{thread_id}"), value.clone())
                            .unwrap();

                        // Immediately read back - should see the value we just wrote
                        let read_value = store_clone
                            .get("shared_flow", &format!("key_{thread_id}"))
                            .unwrap();

                        // This is the critical assertion: we should ALWAYS see our own write
                        assert_eq!(
                            read_value,
                            Some(value.clone()),
                            "Thread {thread_id} iteration {i}: Failed to read back own write"
                        );

                        // Also verify exists() sees it
                        assert!(
                            store_clone
                                .exists("shared_flow", &format!("key_{thread_id}"))
                                .unwrap(),
                            "Thread {thread_id} iteration {i}: exists() returned false after set"
                        );
                    }
                })
            })
            .collect();

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify final state: all keys should exist with their last written values
        for thread_id in 0..num_threads {
            let final_value = store
                .get("shared_flow", &format!("key_{thread_id}"))
                .unwrap();
            assert!(
                final_value.is_some(),
                "Thread {thread_id} final value missing"
            );
        }
    }

    #[test]
    fn test_concurrent_increment_contention() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryFlowStore::new(300));
        let num_threads = 10;
        let increments_per_thread = 100;

        // Spawn multiple threads that all increment the same counter
        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let store_clone = Arc::clone(&store);
                thread::spawn(move || {
                    for _ in 0..increments_per_thread {
                        store_clone.increment("shared_flow", "counter").unwrap();
                    }
                })
            })
            .collect();

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // The counter should equal exactly num_threads * increments_per_thread
        let final_value = store.get("shared_flow", "counter").unwrap();
        let expected = num_threads * increments_per_thread;

        assert_eq!(
            final_value,
            Some(json!(expected)),
            "Concurrent increments lost updates: expected {expected}, got {final_value:?}"
        );
    }

    // ===== increment_by (issue #358) =====

    #[test]
    fn increment_by_starts_at_zero_and_accumulates() {
        let store = InMemoryFlowStore::new(300);
        assert_eq!(store.increment_by("f", "k", 5).unwrap(), 5);
        assert_eq!(store.increment_by("f", "k", 5).unwrap(), 10);
    }

    #[test]
    fn increment_by_negative_decrements() {
        let store = InMemoryFlowStore::new(300);
        store.increment_by("f", "k", 10).unwrap();
        assert_eq!(store.increment_by("f", "k", -3).unwrap(), 7);
    }

    #[test]
    fn increment_delegates_to_increment_by_one() {
        let store = InMemoryFlowStore::new(300);
        assert_eq!(store.increment("f", "k").unwrap(), 1);
        assert_eq!(store.increment_by("f", "k", 1).unwrap(), 2);
    }

    #[test]
    fn increment_by_overflow_errors_not_panics() {
        let store = InMemoryFlowStore::new(300);
        store.set("f", "k", json!(i64::MAX)).unwrap();
        let result = store.increment_by("f", "k", 1);
        assert!(
            result.is_err(),
            "increment_by past i64::MAX must error, not panic/wrap"
        );
        // The stored value must be untouched by the failed op.
        assert_eq!(store.get("f", "k").unwrap(), Some(json!(i64::MAX)));
    }

    // ===== compare_and_set (issue #311) =====

    #[test]
    fn cas_expected_absent_applies() {
        let store = InMemoryFlowStore::new(300);
        let outcome = store
            .compare_and_set("f", "state", None, json!("paid"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Applied));
        assert_eq!(store.get("f", "state").expect("get"), Some(json!("paid")));
    }

    #[test]
    fn cas_expected_present_applies() {
        let store = InMemoryFlowStore::new(300);
        store.set("f", "state", json!("Started")).expect("set");
        let outcome = store
            .compare_and_set("f", "state", Some(&json!("Started")), json!("paid"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Applied));
        assert_eq!(store.get("f", "state").expect("get"), Some(json!("paid")));
    }

    #[test]
    fn cas_conflict_returns_current() {
        let store = InMemoryFlowStore::new(300);
        store.set("f", "state", json!("shipped")).expect("set");
        let outcome = store
            .compare_and_set("f", "state", Some(&json!("Started")), json!("paid"))
            .expect("cas");
        match outcome {
            CasOutcome::Conflict(current) => assert_eq!(current, Some(json!("shipped"))),
            CasOutcome::Applied => panic!("must conflict"),
        }
        assert_eq!(
            store.get("f", "state").expect("get"),
            Some(json!("shipped")),
            "conflict must not write"
        );

        let outcome = store
            .compare_and_set("f", "absent", Some(&json!("Started")), json!("paid"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Conflict(None)));
    }

    // AC1: N racers, one CAS each on the same expected state — exactly one Applied,
    // the rest Conflict, final state legal.
    #[test]
    fn cas_race_exactly_one_winner() {
        use std::sync::Barrier;
        use std::thread;

        let store = Arc::new(InMemoryFlowStore::new(300));
        store.set("f", "state", json!("Started")).expect("seed");
        let racers = 64;
        let barrier = Arc::new(Barrier::new(racers));

        let handles: Vec<_> = (0..racers)
            .map(|i| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let outcome = store
                        .compare_and_set(
                            "f",
                            "state",
                            Some(&json!("Started")),
                            json!(format!("paid-by-{i}")),
                        )
                        .expect("cas");
                    matches!(outcome, CasOutcome::Applied)
                })
            })
            .collect();

        let applied = handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .filter(|won| *won)
            .count();
        assert_eq!(applied, 1, "exactly one racer must win the CAS");

        let final_state = store.get("f", "state").expect("get").expect("present");
        let s = final_state.as_str().expect("string state");
        assert!(s.starts_with("paid-by-"), "final state legal, got {s}");
    }
}
