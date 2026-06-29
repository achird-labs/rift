use crate::extensions::flow_state::FlowStore;
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
        if let Some((_, expiry)) = data.get(key) {
            if is_expired_fn(expiry) {
                data.remove(key);
            }
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
        let key_str = self.make_key(flow_id, key);
        let expiry = SystemTime::now() + self.default_ttl;
        let mut data = self.data.write();

        // Opportunistically clean up this specific key if expired
        Self::cleanup_on_write(&mut data, &key_str, |exp| self.is_expired(exp));

        let new_value = match data.get(&key_str) {
            Some((Value::Number(n), _)) if n.is_i64() => n.as_i64().unwrap_or(0) + 1,
            _ => 1,
        };

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
