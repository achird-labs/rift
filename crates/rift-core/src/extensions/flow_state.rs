use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::sync::Arc;

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

    /// Set TTL for all keys under a flow_id
    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()>;
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
        tracing::warn!("NoOpFlowStore: increment called but no state tracking available. Configure flow_state for stateful scripts.");
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
        assert!(store
            .set("flow", "key", json!({"nested": {"deep": "value"}}))
            .is_ok());
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
