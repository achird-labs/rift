use crate::extensions::flow_state::{CasOutcome, FlowStore};
use anyhow::Result;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// A single stored entry: its value and optional expiry instant.
type Entry = (Value, Option<SystemTime>);
/// One flow's key→entry map.
type FlowMap = HashMap<String, Entry>;
/// The whole store, keyed by `flow_id` then `key`. Nesting (vs. a flat `flow:{id}:{key}` map,
/// issue #483) lets `set_ttl` touch only the target flow's keys instead of scanning every entry,
/// and lets the reaper drop a whole expired flow in one step.
type Store = HashMap<String, FlowMap>;

/// Run a full expired-entry sweep once every this many write operations (issue #483). Amortizes
/// the O(total) reap to O(1) per write while bounding how long an expired entry lingers.
const SWEEP_INTERVAL: usize = 256;

/// In-memory implementation of FlowStore
///
/// This implementation stores flow state in a nested HashMap with automatic TTL expiration.
/// Useful for testing, development, and single-instance deployments.
pub struct InMemoryFlowStore {
    data: Arc<RwLock<Store>>,
    default_ttl: Duration,
    /// Store-growing writes since the last sweep; drives the amortized reaper (issue #483). Only
    /// `set`/`increment_by`/applied-CAS count — the paths that can add an entry. `delete`, `set_ttl`,
    /// and a conflicting CAS never grow the store, so they deliberately don't advance the counter
    /// (delete reclaims directly; a conflicting CAS wrote nothing).
    writes_since_sweep: AtomicUsize,
}

impl InMemoryFlowStore {
    pub fn new(default_ttl_seconds: u64) -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            default_ttl: Duration::from_secs(default_ttl_seconds),
            writes_since_sweep: AtomicUsize::new(0),
        }
    }

    /// List every non-expired key currently stored under `flow_id`, for `rift script run`'s
    /// post-execution state dump (issue #360 Item 2) — the [`FlowStore`] trait itself has no
    /// enumeration method (a real backend like Redis may not make that cheap), but the CLI works
    /// with a concrete `InMemoryFlowStore` fixture, so an inherent method here is enough.
    pub fn keys_for_flow(&self, flow_id: &str) -> Vec<String> {
        let data = self.data.read();
        match data.get(flow_id) {
            Some(flow) => flow
                .iter()
                .filter(|(_, (_, expiry))| !Self::is_expired(expiry))
                .map(|(key, _)| key.clone())
                .collect(),
            None => Vec::new(),
        }
    }

    fn is_expired(expiry: &Option<SystemTime>) -> bool {
        matches!(expiry, Some(exp) if SystemTime::now() > *exp)
    }

    /// Remove a flow's entry for `key` if it has expired, so a subsequent read sees it as absent.
    /// Called with the write lock held; mirrors the pre-#483 opportunistic same-key cleanup.
    fn remove_if_expired(flow: &mut FlowMap, key: &str) {
        if let Some((_, expiry)) = flow.get(key)
            && Self::is_expired(expiry)
        {
            flow.remove(key);
        }
    }

    /// Count one write toward the amortized reaper and, every [`SWEEP_INTERVAL`] writes, drop
    /// every expired entry (and any flow left empty). Called with the write lock already held.
    fn maybe_sweep(&self, data: &mut Store) {
        if self.writes_since_sweep.fetch_add(1, Ordering::Relaxed) + 1 >= SWEEP_INTERVAL {
            self.writes_since_sweep.store(0, Ordering::Relaxed);
            Self::sweep_expired(data);
        }
    }

    /// Drop every expired entry across all flows, and remove any flow map left empty. O(total),
    /// but run only once per [`SWEEP_INTERVAL`] writes.
    fn sweep_expired(data: &mut Store) {
        data.retain(|_flow_id, flow| {
            flow.retain(|_key, (_, expiry)| !Self::is_expired(expiry));
            !flow.is_empty()
        });
    }

    /// Total entries currently held (including any expired-but-not-yet-swept), for reaper tests.
    #[cfg(test)]
    fn stored_entry_count(&self) -> usize {
        self.data.read().values().map(HashMap::len).sum()
    }

    /// Number of flow maps currently held, for reaper tests — the real high-cardinality growth
    /// vector is the outer map's key count, which `stored_entry_count` (a sum of inner lens) can't
    /// see (an empty flow map contributes 0).
    #[cfg(test)]
    fn stored_flow_count(&self) -> usize {
        self.data.read().len()
    }
}

impl FlowStore for InMemoryFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
        // Use read lock for concurrent read access
        let data = self.data.read();
        match data.get(flow_id).and_then(|flow| flow.get(key)) {
            Some((value, expiry)) if !Self::is_expired(expiry) => Ok(Some(value.clone())),
            _ => Ok(None),
        }
    }

    fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()> {
        let expiry = SystemTime::now() + self.default_ttl;
        let mut data = self.data.write();

        data.entry(flow_id.to_string())
            .or_default()
            .insert(key.to_string(), (value, Some(expiry)));
        self.maybe_sweep(&mut data);
        Ok(())
    }

    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        // Use read lock for concurrent read access
        let data = self.data.read();
        match data.get(flow_id).and_then(|flow| flow.get(key)) {
            Some((_, expiry)) if !Self::is_expired(expiry) => Ok(true),
            _ => Ok(false),
        }
    }

    fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
        let mut data = self.data.write();
        if let Some(flow) = data.get_mut(flow_id) {
            flow.remove(key);
            if flow.is_empty() {
                data.remove(flow_id);
            }
        }
        Ok(())
    }

    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        self.increment_by(flow_id, key, 1)
    }

    /// Atomic under the single write lock (issue #358), like `increment`: the read-modify-write
    /// happens with the lock held, so no interleaving window with a concurrent increment/set.
    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        let expiry = SystemTime::now() + self.default_ttl;
        let mut data = self.data.write();
        let flow = data.entry(flow_id.to_string()).or_default();

        // Opportunistically clean up this specific key if expired, so a stale value isn't summed.
        Self::remove_if_expired(flow, key);

        let current = match flow.get(key) {
            Some((Value::Number(n), _)) if n.is_i64() => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        // `checked_add` so an overflow near i64::MAX errors (fail-loud) instead of panicking in
        // debug / wrapping in release — matching Redis's INCRBY, which also errors on overflow.
        let new_value = current.checked_add(by).ok_or_else(|| {
            anyhow::anyhow!("increment_by overflow: {current} + {by} exceeds i64 range")
        })?;

        flow.insert(
            key.to_string(),
            (Value::Number(new_value.into()), Some(expiry)),
        );
        self.maybe_sweep(&mut data);
        Ok(new_value)
    }

    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()> {
        let new_expiry =
            SystemTime::now() + Duration::from_secs(u64::try_from(ttl_seconds).unwrap_or(0));
        let mut data = self.data.write();

        // Nested keying (issue #483): touch only this flow's entries, not the whole store.
        if let Some(flow) = data.get_mut(flow_id) {
            for (_, expiry) in flow.values_mut() {
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
        let mut data = self.data.write();
        let flow = data.entry(flow_id.to_string()).or_default();
        Self::remove_if_expired(flow, key);

        let current = flow.get(key).map(|(value, _)| value);
        if current == expected {
            let expiry = SystemTime::now() + self.default_ttl;
            flow.insert(key.to_string(), (new, Some(expiry)));
            self.maybe_sweep(&mut data);
            Ok(CasOutcome::Applied)
        } else {
            let conflict = current.cloned();
            // A CAS that didn't write can still have created an empty flow via `entry().or_default()`
            // above; drop it so a failed compare never leaks a flow map.
            if flow.is_empty() {
                data.remove(flow_id);
            }
            Ok(CasOutcome::Conflict(conflict))
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

    // ===== reaping + targeted set_ttl (issue #483) =====

    // The amortized sweeper must actually reclaim expired entries across many short-lived flows,
    // so a long-running store doesn't grow without bound. Before this fix expired entries were
    // only ever removed on a same-key rewrite, so high-cardinality flow ids leaked forever.
    #[test]
    fn sweeper_reaps_expired_entries_across_flows() {
        let store = InMemoryFlowStore::new(1); // 1s TTL

        // One entry in each of many distinct, never-rewritten flows.
        for i in 0..300 {
            store.set(&format!("flow-{i}"), "k", json!(i)).unwrap();
        }
        assert_eq!(store.stored_entry_count(), 300);

        // Let them all expire.
        std::thread::sleep(Duration::from_secs(2));

        // Drive enough writes (to a single overwritten key) to trigger at least one sweep now
        // that the 300 are expired.
        for _ in 0..(SWEEP_INTERVAL * 2) {
            store.set("sink", "k", json!(1)).unwrap();
        }

        // Only the live sink entry survives; the 300 expired flows were reaped — asserted on both
        // the entry count AND the outer flow-map count (the real high-cardinality growth vector:
        // an empty flow map left in the outer HashMap contributes 0 to stored_entry_count).
        assert_eq!(
            store.stored_entry_count(),
            1,
            "expired entries across distinct flows must be reaped, leaving only the live one"
        );
        assert_eq!(
            store.stored_flow_count(),
            1,
            "empty flow maps for the expired flows must be dropped from the outer map"
        );
    }

    // delete of a flow's sole key must drop the flow map from the outer store immediately, not
    // leave an empty map to accumulate (issue #483's growth vector).
    #[test]
    fn delete_last_key_removes_the_flow_map() {
        let store = InMemoryFlowStore::new(300);
        store.set("f", "k", json!("v")).unwrap();
        assert_eq!(store.stored_flow_count(), 1);
        store.delete("f", "k").unwrap();
        assert_eq!(
            store.stored_flow_count(),
            0,
            "deleting a flow's last key must remove its flow map, not leave it empty"
        );
    }

    // A conflicting CAS on a brand-new flow_id must not leak the empty flow map that
    // `entry().or_default()` transiently created.
    #[test]
    fn cas_conflict_on_new_flow_leaves_no_flow_map() {
        let store = InMemoryFlowStore::new(300);
        // Expect a value on a flow that doesn't exist -> current None != expected -> Conflict.
        let outcome = store
            .compare_and_set("ghost", "k", Some(&json!("nope")), json!("x"))
            .expect("cas");
        assert!(matches!(outcome, CasOutcome::Conflict(None)));
        assert_eq!(
            store.stored_flow_count(),
            0,
            "a conflicting CAS on a new flow must not leak an empty flow map"
        );
    }

    // set_ttl must extend only the target flow's entries — and, per #483, do so without scanning
    // unrelated flows. Verified behaviorally: a sibling flow keeps its short TTL and expires.
    #[test]
    fn set_ttl_only_affects_target_flow() {
        let store = InMemoryFlowStore::new(1); // 1s default TTL
        store.set("keep", "k", json!("v")).unwrap();
        store.set("drop", "k", json!("v")).unwrap();

        // Extend only "keep" well past the sleep below.
        store.set_ttl("keep", 100).unwrap();

        std::thread::sleep(Duration::from_secs(2));

        assert!(
            store.exists("keep", "k").unwrap(),
            "set_ttl should have extended the target flow"
        );
        assert!(
            !store.exists("drop", "k").unwrap(),
            "a sibling flow must keep its original (now expired) TTL"
        );
    }
}
