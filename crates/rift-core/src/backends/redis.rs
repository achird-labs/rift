use crate::extensions::flow_state::{CasOutcome, FlowStore};
use anyhow::{Context, Result};
use redis::{Commands, Connection};
use serde_json::Value;
use std::sync::Mutex;

/// Redis implementation of FlowStore using synchronous blocking client
///
/// This implementation uses a blocking Redis client with r2d2 connection pooling.
/// The synchronous nature avoids async bridging deadlocks when called from scripts.
///
/// # Compatibility
///
/// - Redis 6.x, 7.x: Fully supported
/// - Valkey: Likely compatible but not officially supported
///
/// Simple connection manager for Redis
struct RedisConnectionManager {
    client: redis::Client,
}

impl RedisConnectionManager {
    fn new(client: redis::Client) -> Self {
        Self { client }
    }
}

impl r2d2::ManageConnection for RedisConnectionManager {
    type Connection = Mutex<Connection>;
    type Error = redis::RedisError;

    fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let conn = self.client.get_connection()?;
        Ok(Mutex::new(conn))
    }

    fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        redis::cmd("PING").query(conn.get_mut().unwrap())
    }

    fn has_broken(&self, _conn: &mut Self::Connection) -> bool {
        false
    }
}

pub struct RedisFlowStore {
    pool: r2d2::Pool<RedisConnectionManager>,
    key_prefix: String,
    default_ttl_seconds: i64,
}

impl RedisFlowStore {
    /// Create a new Redis flow store
    ///
    /// # Arguments
    /// * `url` - Redis connection URL (e.g. "redis://localhost:6379")
    /// * `pool_size` - Connection pool size
    /// * `key_prefix` - Prefix for all keys (e.g. "rift:")
    /// * `default_ttl_seconds` - Default TTL for keys
    pub fn new(
        url: &str,
        pool_size: usize,
        key_prefix: String,
        default_ttl_seconds: i64,
    ) -> Result<Self> {
        let client = redis::Client::open(url).context("Failed to parse Redis URL")?;

        let manager = RedisConnectionManager::new(client);

        let pool = r2d2::Pool::builder()
            .max_size(pool_size as u32)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build(manager)
            .context("Failed to create Redis connection pool")?;

        // Test connection with PING
        {
            let conn = pool.get().context("Failed to get connection from pool")?;
            let _: String = redis::cmd("PING")
                .query(&mut *conn.lock().unwrap())
                .context("Failed to PING Redis")?;
        }

        tracing::info!(
            "Connected to Redis with prefix={}, ttl={}s, pool_size={}",
            key_prefix,
            default_ttl_seconds,
            pool_size
        );

        Ok(Self {
            pool,
            key_prefix,
            default_ttl_seconds,
        })
    }

    /// Make a full key with prefix and flow_id
    fn make_key(&self, flow_id: &str, key: &str) -> String {
        format!("{}flow:{}:{}", self.key_prefix, flow_id, key)
    }
}

/// Wrap a Redis op failure so response boundaries map it to a structured 503 (issue
/// #318): annotate the failed op, then attach `BackendUnavailable`. Serde failures are
/// NOT wrapped — malformed stored data is corruption, not backend unavailability.
fn backend_err(op: &'static str, err: impl std::fmt::Display) -> anyhow::Error {
    crate::extensions::decorate::annotate(op, err.to_string());
    anyhow::Error::new(crate::extensions::decorate::BackendUnavailable {
        feature: "flowState",
        detail: format!("{op}: {err}"),
    })
}

impl FlowStore for RedisFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> Result<Option<Value>> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        let value: Option<String> = conn
            .lock()
            .unwrap()
            .get(&key_str)
            .map_err(|e| backend_err("flowStore.get", e))?;

        if let Some(json_str) = value {
            let val = serde_json::from_str(&json_str).context("Failed to parse JSON from Redis")?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    fn set(&self, flow_id: &str, key: &str, value: Value) -> Result<()> {
        let key_str = self.make_key(flow_id, key);
        let json_str =
            serde_json::to_string(&value).context("Failed to serialize value to JSON")?;

        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        // SET with TTL using SETEX
        let _: () = redis::cmd("SETEX")
            .arg(&key_str)
            .arg(self.default_ttl_seconds)
            .arg(json_str)
            .query(&mut *conn.lock().unwrap())
            .map_err(|e| backend_err("flowStore.set", e))?;

        Ok(())
    }

    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        let count: i64 = conn
            .lock()
            .unwrap()
            .exists(&key_str)
            .map_err(|e| backend_err("flowStore.exists", e))?;

        Ok(count > 0)
    }

    fn delete(&self, flow_id: &str, key: &str) -> Result<()> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        let _: () = conn
            .lock()
            .unwrap()
            .del(&key_str)
            .map_err(|e| backend_err("flowStore.delete", e))?;

        Ok(())
    }

    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        self.increment_by(flow_id, key, 1)
    }

    /// Atomic via Redis's own `INCRBY` (issue #358) — a single round trip, so there's no
    /// interleaving window with a concurrent increment/set the way a get-then-set fallback would
    /// have.
    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;
        let mut conn_guard = conn.lock().unwrap();

        // INCRBY returns the new value
        let new_value: i64 = conn_guard
            .incr(&key_str, by)
            .map_err(|e| backend_err("flowStore.incrementBy", e))?;

        // Set TTL on the key (INCRBY doesn't reset TTL)
        let _: () = redis::cmd("EXPIRE")
            .arg(&key_str)
            .arg(self.default_ttl_seconds)
            .query(&mut *conn_guard)
            .map_err(|e| backend_err("flowStore.expire", e))?;

        Ok(new_value)
    }

    /// Single-round-trip atomic CAS via a server-side Lua script (issue #311): compare
    /// and SETEX happen inside one EVAL, so no WATCH/MULTI and no interleaving window.
    /// Values compare as their canonical JSON strings — the same encoding `set` writes.
    fn compare_and_set(
        &self,
        flow_id: &str,
        key: &str,
        expected: Option<&Value>,
        new: Value,
    ) -> Result<CasOutcome> {
        // Lua false/nil TERMINATES a Redis table reply, so absence is encoded as '' —
        // unambiguous because stored payloads are always JSON (never the empty string).
        const CAS_SCRIPT: &str = r#"
local current = redis.call('GET', KEYS[1])
local matches
if ARGV[1] == '0' then
  matches = (current == false)
else
  matches = (current == ARGV[2])
end
if matches then
  redis.call('SETEX', KEYS[1], tonumber(ARGV[4]), ARGV[3])
  return {1, current or ''}
else
  return {0, current or ''}
end
"#;
        let key_str = self.make_key(flow_id, key);
        let expected_json = expected
            .map(|v| serde_json::to_string(v).context("Failed to serialize expected to JSON"))
            .transpose()?
            .unwrap_or_default();
        let new_json = serde_json::to_string(&new).context("Failed to serialize value to JSON")?;

        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;
        let (applied, current): (i64, String) = redis::cmd("EVAL")
            .arg(CAS_SCRIPT)
            .arg(1)
            .arg(&key_str)
            .arg(if expected.is_some() { "1" } else { "0" })
            .arg(&expected_json)
            .arg(&new_json)
            .arg(self.default_ttl_seconds)
            .query(&mut *conn.lock().unwrap())
            .map_err(|e| backend_err("flowStore.compareAndSet", e))?;

        if applied == 1 {
            Ok(CasOutcome::Applied)
        } else {
            let current = if current.is_empty() {
                None
            } else {
                Some(serde_json::from_str(&current).context("Failed to parse JSON from Redis")?)
            };
            Ok(CasOutcome::Conflict(current))
        }
    }

    fn set_ttl(&self, flow_id: &str, ttl_seconds: i64) -> Result<()> {
        // For now, just log a debug message - individual keys get TTL via set() and increment()
        // To fully implement this, we'd need to SCAN for all keys matching the pattern
        // and EXPIRE each one, which is expensive
        tracing::debug!(
            "set_ttl called for flow_id={} with ttl={}s - individual operations already set TTL",
            flow_id,
            ttl_seconds
        );

        // Return success since individual operations already handle TTL
        Ok(())
    }
}

/// Health check for Redis connection
#[allow(dead_code, private_interfaces)]
pub(crate) fn health_check(pool: &r2d2::Pool<RedisConnectionManager>) -> Result<bool> {
    let conn = pool.get().context("Failed to get connection from pool")?;

    let mut guard = conn.lock().unwrap();
    match redis::cmd("PING").query::<String>(&mut *guard) {
        Ok(_) => Ok(true),
        Err(e) => {
            tracing::warn!("Redis health check failed: {}", e);
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every Redis op failure funnels through backend_err, so this pins the whole file's
    // error shape: typed, downcastable, structured-503-able (issue #318).
    #[test]
    fn backend_err_is_downcastable_to_backend_unavailable() {
        let err = backend_err("flowStore.get", "connection refused");
        let unavailable = err
            .downcast_ref::<crate::extensions::decorate::BackendUnavailable>()
            .expect("typed error");
        assert_eq!(unavailable.feature, "flowState");
        assert!(unavailable.detail.contains("flowStore.get"));
        assert!(unavailable.detail.contains("connection refused"));
    }
}
