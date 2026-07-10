//! Integration tests for RedisFlowStore using testcontainers.
//!
//! These tests start a throwaway Redis container via testcontainers, so **Docker must be
//! running**. When Docker is not available they are **skipped** locally (with a note on stderr)
//! rather than hanging and failing — so `cargo test -p rift-http-proxy` is green on a dev machine
//! without Docker. In CI (`CI`/`GITHUB_ACTIONS` set) an unavailable Docker is a hard failure, so
//! coverage is never silently lost. To run them locally, start Docker (e.g. `colima start` or
//! Docker Desktop) and re-run.

#[cfg(feature = "redis-backend")]
mod tests {
    use rift_http_proxy::backends::RedisFlowStore;
    use rift_http_proxy::flow_state::FlowStore;
    use serde_json::json;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis;

    /// Whether a Docker daemon is reachable, probed once. testcontainers needs Docker to start the
    /// Redis container; probing up front lets us skip fast instead of waiting out its ~120s
    /// container-start retry. The probe itself is bounded to 5s and killed if it exceeds that,
    /// because `docker info` can itself hang when the CLI is installed but the daemon is down.
    fn docker_available() -> bool {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let Ok(mut child) = Command::new("docker")
                .arg("info")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                return false; // docker CLI not installed
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success(),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false; // daemon unreachable / probe hung
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => return false,
                }
            }
        })
    }

    /// Start a Redis container and build a store against it. Returns `None` when Docker is
    /// unavailable so the caller can skip — except under `CI`, where Docker is required and its
    /// absence is a hard failure (so CI never silently loses Redis coverage).
    async fn setup(ttl: i64) -> Option<(testcontainers::ContainerAsync<Redis>, RedisFlowStore)> {
        if !docker_available() {
            let in_ci = std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok();
            assert!(
                !in_ci,
                "Docker is required for the Redis integration tests in CI (CI/GITHUB_ACTIONS set) but is not available"
            );
            eprintln!(
                "skipping Redis integration test: Docker is not available (start Docker to run these locally)"
            );
            return None;
        }
        let container = Redis::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let store = RedisFlowStore::new(
            &format!("redis://127.0.0.1:{port}"),
            5,
            "test:".to_string(),
            ttl,
        )
        .unwrap();
        Some((container, store))
    }

    #[tokio::test]
    async fn test_redis_get_set() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };

        store.set("flow1", "key1", json!("value1")).unwrap();
        let value = store.get("flow1", "key1").unwrap();
        assert_eq!(value, Some(json!("value1")));

        store.delete("flow1", "key1").unwrap();
    }

    #[tokio::test]
    async fn test_redis_increment() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };

        let v1 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v1, 1);

        let v2 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v2, 2);

        let v3 = store.increment("flow1", "counter").unwrap();
        assert_eq!(v3, 3);

        store.delete("flow1", "counter").unwrap();
    }

    #[tokio::test]
    async fn test_redis_ttl() {
        let Some((_container, store)) = setup(2).await else {
            return;
        };

        store.set("flow1", "key1", json!("value1")).unwrap();
        assert!(store.exists("flow1", "key1").unwrap());

        // Wait for expiry
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        assert!(!store.exists("flow1", "key1").unwrap());
    }

    #[tokio::test]
    async fn test_redis_exists_delete() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };

        store.set("flow1", "key1", json!("value1")).unwrap();
        assert!(store.exists("flow1", "key1").unwrap());

        store.delete("flow1", "key1").unwrap();
        assert!(!store.exists("flow1", "key1").unwrap());
    }

    // ===== compare_and_set (issue #311) — single-round-trip Lua CAS =====

    use rift_http_proxy::flow_state::CasOutcome;

    #[tokio::test]
    async fn test_redis_cas_expected_absent_applies() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };

        let outcome = store
            .compare_and_set("casf", "state", None, json!("paid"))
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Applied));
        assert_eq!(store.get("casf", "state").unwrap(), Some(json!("paid")));

        store.delete("casf", "state").unwrap();
    }

    // SETEX inside the CAS script must carry the store TTL: a CAS-applied key expires
    // like a set() key (a dropped TTL arg would mean unbounded key growth in Redis).
    #[tokio::test]
    async fn test_redis_cas_applied_key_expires() {
        let Some((_container, store)) = setup(2).await else {
            return;
        };

        let outcome = store
            .compare_and_set("casttl", "state", None, json!("paid"))
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Applied));
        assert!(store.exists("casttl", "state").unwrap());

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(
            store.get("casttl", "state").unwrap(),
            None,
            "CAS-applied key must expire with the store TTL"
        );
    }

    // Issue #475: increment_by folds INCRBY + EXPIRE into one EVAL. Mirror the CAS-expiry test to
    // pin that the EXPIRE arg isn't dropped — a lost EXPIRE would mean unbounded key growth.
    #[tokio::test]
    async fn test_redis_increment_by_key_expires() {
        let Some((_container, store)) = setup(2).await else {
            return;
        };

        assert_eq!(store.increment_by("incttl", "n", 5).unwrap(), 5);
        assert!(store.exists("incttl", "n").unwrap());

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(
            store.get("incttl", "n").unwrap(),
            None,
            "increment_by key must expire with the store TTL (EXPIRE folded into the EVAL)"
        );
    }

    #[tokio::test]
    async fn test_redis_cas_expected_present_and_conflict() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };
        store.set("casf2", "state", json!("Started")).unwrap();

        let outcome = store
            .compare_and_set("casf2", "state", Some(&json!("Started")), json!("paid"))
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Applied));

        // Now the state is "paid": expecting "Started" must conflict and return "paid".
        let outcome = store
            .compare_and_set("casf2", "state", Some(&json!("Started")), json!("shipped"))
            .unwrap();
        match outcome {
            CasOutcome::Conflict(current) => assert_eq!(current, Some(json!("paid"))),
            CasOutcome::Applied => panic!("must conflict"),
        }
        assert_eq!(
            store.get("casf2", "state").unwrap(),
            Some(json!("paid")),
            "conflict must not write"
        );

        // Expecting absent while present also conflicts.
        let outcome = store
            .compare_and_set("casf2", "state", None, json!("shipped"))
            .unwrap();
        assert!(matches!(outcome, CasOutcome::Conflict(Some(_))));

        store.delete("casf2", "state").unwrap();
    }
}
