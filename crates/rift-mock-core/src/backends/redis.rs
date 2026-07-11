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
        // A poisoned lock means a previous caller panicked mid-command, so the connection's
        // protocol stream may be inconsistent; report it invalid so r2d2 discards it.
        if conn.is_poisoned() {
            return Err(redis::RedisError::from((
                redis::ErrorKind::IoError,
                "pooled Redis connection mutex poisoned by a previous panic",
            )));
        }
        redis::cmd("PING").query(conn.get_mut().unwrap_or_else(|p| p.into_inner()))
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        conn.is_poisoned()
    }
}

/// Lock a connection mutex, recovering from a poisoned lock instead of panicking. A poison only
/// means a previous caller panicked while holding the lock; the pool's `has_broken`/`is_valid`
/// checks then discard the connection, so recovering here turns a one-off panic into a handled
/// error rather than a repeating-panic cascade (issue #540).
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
                .query(&mut *lock_recover(&conn))
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

    /// `SCAN MATCH` glob for every key under a flow (issue #530). Mirrors [`Self::make_key`] with a
    /// trailing `*` for the key part, so `set_ttl` and `clear_flow` operate on exactly the flow's own
    /// namespace.
    ///
    /// `flow_id` is glob-**escaped** before interpolation: it is request-derived (a path/header/body
    /// capture, or the admin `DELETE .../flow-state/:flow_id` path segment), so a raw `*`/`?`/`[` in
    /// it would otherwise make these destructive ops match — and wipe/expire — *sibling* flows' keys.
    /// Escaping keeps the match literal, matching the in-memory backend's exact-string keying.
    fn flow_scan_pattern(&self, flow_id: &str) -> String {
        format!("{}flow:{}:*", self.key_prefix, escape_glob(flow_id))
    }
}

/// Escape the Redis glob metacharacters (`*`, `?`, `[`, `]`, `\`) in `s` so it matches literally
/// inside a `SCAN`/`KEYS` `MATCH` pattern (issue #530). Redis pattern matching treats `\x` as a
/// literal `x`, so a backslash prefix neutralizes each metacharacter.
fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

        let value: Option<String> = lock_recover(&conn)
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
            .query(&mut *lock_recover(&conn))
            .map_err(|e| backend_err("flowStore.set", e))?;

        Ok(())
    }

    fn exists(&self, flow_id: &str, key: &str) -> Result<bool> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        let count: i64 = lock_recover(&conn)
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

        let _: () = lock_recover(&conn)
            .del(&key_str)
            .map_err(|e| backend_err("flowStore.delete", e))?;

        Ok(())
    }

    fn increment(&self, flow_id: &str, key: &str) -> Result<i64> {
        self.increment_by(flow_id, key, 1)
    }

    /// Atomic INCRBY + EXPIRE in a single round trip via a server-side Lua script (issue #475),
    /// mirroring `compare_and_set`. `INCRBY` alone doesn't (re)set the TTL, and issuing a separate
    /// `EXPIRE` cost a second RTT per increment; folding both into one `EVAL` halves the round
    /// trips while staying atomic.
    fn increment_by(&self, flow_id: &str, key: &str, by: i64) -> Result<i64> {
        const INCR_SCRIPT: &str = r#"
local v = redis.call('INCRBY', KEYS[1], ARGV[1])
redis.call('EXPIRE', KEYS[1], ARGV[2])
return v
"#;
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;

        let new_value: i64 = redis::cmd("EVAL")
            .arg(INCR_SCRIPT)
            .arg(1)
            .arg(&key_str)
            .arg(by)
            .arg(self.default_ttl_seconds)
            .query(&mut *lock_recover(&conn))
            .map_err(|e| backend_err("flowStore.incrementBy", e))?;

        Ok(new_value)
    }

    /// The synchronous r2d2/`redis::Connection` client blocks the calling thread, so the request
    /// path must offload these calls to `spawn_blocking` (issue #475).
    fn is_blocking(&self) -> bool {
        true
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
            .query(&mut *lock_recover(&conn))
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
        // Issue #530: re-stamp the TTL of every key currently in the flow via SCAN + EXPIRE. This
        // replaces the former silent no-op — a script calling `ctx.state.ttl(3600)` on Redis now
        // actually extends its keys, matching the in-memory backend. A non-positive TTL drops every
        // key (Redis `EXPIRE` semantics). O(keys-in-flow), but this is an explicit script call, not
        // on the per-request hot path.
        let pattern = self.flow_scan_pattern(flow_id);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;
        let mut guard = lock_recover(&conn);
        let mut cursor: u64 = 0;
        loop {
            let (next, keys) = scan_batch(&mut guard, cursor, &pattern, "flowStore.setTtl")?;
            for k in &keys {
                if ttl_seconds <= 0 {
                    let _: i64 = redis::cmd("DEL")
                        .arg(k)
                        .query(&mut *guard)
                        .map_err(|e| backend_err("flowStore.setTtl", e))?;
                } else {
                    let _: i64 = redis::cmd("EXPIRE")
                        .arg(k)
                        .arg(ttl_seconds)
                        .query(&mut *guard)
                        .map_err(|e| backend_err("flowStore.setTtl", e))?;
                }
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        Ok(())
    }

    fn set_key_ttl(&self, flow_id: &str, key: &str, ttl_seconds: i64) -> Result<bool> {
        let key_str = self.make_key(flow_id, key);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;
        let mut guard = lock_recover(&conn);
        if ttl_seconds <= 0 {
            // Redis `EXPIRE key <=0` deletes the key; use DEL explicitly and report whether it
            // existed (issue #530).
            let deleted: i64 = redis::cmd("DEL")
                .arg(&key_str)
                .query(&mut *guard)
                .map_err(|e| backend_err("flowStore.setKeyTtl", e))?;
            Ok(deleted > 0)
        } else {
            // EXPIRE returns 1 if the timeout was set, 0 if the key does not exist — exactly the
            // "existed?" bool this method promises.
            let applied: i64 = redis::cmd("EXPIRE")
                .arg(&key_str)
                .arg(ttl_seconds)
                .query(&mut *guard)
                .map_err(|e| backend_err("flowStore.setKeyTtl", e))?;
            Ok(applied == 1)
        }
    }

    fn clear_flow(&self, flow_id: &str) -> Result<()> {
        // SCAN + batched DEL over the flow's key namespace (issue #530). SCAN never blocks the
        // server and tolerates concurrent mutation; DEL of a batch is one round trip.
        let pattern = self.flow_scan_pattern(flow_id);
        let conn = self
            .pool
            .get()
            .map_err(|e| backend_err("flowStore.pool", e))?;
        let mut guard = lock_recover(&conn);
        let mut cursor: u64 = 0;
        loop {
            let (next, keys) = scan_batch(&mut guard, cursor, &pattern, "flowStore.clearFlow")?;
            if !keys.is_empty() {
                let _: i64 = redis::cmd("DEL")
                    .arg(&keys)
                    .query(&mut *guard)
                    .map_err(|e| backend_err("flowStore.clearFlow", e))?;
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        Ok(())
    }
}

/// One `SCAN cursor MATCH pattern COUNT 100` round trip, returning the next cursor and the batch of
/// matched keys. Callers loop until the returned cursor is 0. Extracted so `set_ttl` and
/// `clear_flow` share the identical scan shape (issue #530).
fn scan_batch(
    conn: &mut Connection,
    cursor: u64,
    pattern: &str,
    op: &'static str,
) -> Result<(u64, Vec<String>)> {
    redis::cmd("SCAN")
        .arg(cursor)
        .arg("MATCH")
        .arg(pattern)
        .arg("COUNT")
        .arg(100)
        .query::<(u64, Vec<String>)>(conn)
        .map_err(|e| backend_err(op, e))
}

/// Health check for Redis connection
#[allow(dead_code, private_interfaces)]
pub(crate) fn health_check(pool: &r2d2::Pool<RedisConnectionManager>) -> Result<bool> {
    let conn = pool.get().context("Failed to get connection from pool")?;

    let mut guard = lock_recover(&conn);
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

    // Issue #540: a single panic while a connection lock was held poisoned that Mutex, and every
    // subsequent `.lock().unwrap()` then panicked too — a recoverable poison turned into a
    // repeating-panic cascade. lock_recover must return the guard (and its data) instead.
    #[test]
    fn lock_recover_survives_poisoned_mutex() {
        use std::sync::{Arc, Mutex};
        let m = Arc::new(Mutex::new(vec![1u8, 2, 3]));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(m.is_poisoned(), "precondition: the mutex must be poisoned");

        let guard = lock_recover(&m);
        assert_eq!(&*guard, &vec![1, 2, 3]);
    }

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

    // Issue #530: a flow_id carrying glob metacharacters must be escaped so a SCAN MATCH pattern
    // matches ONLY the literal flow — otherwise clear_flow/set_ttl would destructively match
    // sibling flows. A plain id is passed through unchanged.
    #[test]
    fn escape_glob_neutralizes_scan_metacharacters() {
        assert_eq!(escape_glob("plain"), "plain");
        assert_eq!(escape_glob("a*b"), r"a\*b");
        assert_eq!(escape_glob("a?b"), r"a\?b");
        assert_eq!(escape_glob("a[bc]d"), r"a\[bc\]d");
        assert_eq!(escape_glob(r"a\b"), r"a\\b");
    }
}
