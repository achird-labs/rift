//! Integration tests for RedisFlowStore using testcontainers.
//!
//! These tests start a throwaway Redis container via testcontainers, so **Docker must be
//! running**. When Docker is not available they are **skipped** locally (with a note on stderr)
//! rather than hanging and failing — so `cargo test -p rift-http-proxy` is green on a dev machine
//! without Docker. In CI (`CI`/`GITHUB_ACTIONS` set) a *definitively* absent Docker is a hard
//! failure, so coverage is never silently lost; a probe that merely times out under load proceeds
//! and lets testcontainers decide (issue #649). To run them locally, start Docker (e.g.
//! `colima start` or Docker Desktop) and re-run.

#[cfg(feature = "redis-backend")]
mod tests {
    use rift_http_proxy::backends::RedisFlowStore;
    use rift_http_proxy::flow_state::FlowStore;
    use serde_json::json;
    use std::time::{Duration, Instant};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis;

    /// Outcome of probing for a Docker daemon. `Absent` is *definitive* (CLI missing, or
    /// `docker info` completed with non-success exit); `Timeout` is not — a loaded runner can
    /// legitimately push `docker info` past the deadline while Docker is present and healthy
    /// (issue #649), so the two must never be conflated.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum DockerProbe {
        Available,
        Absent,
        Timeout,
    }

    /// What `setup()` does with a probe outcome.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Disposition {
        Proceed,
        Skip,
        FailCi,
    }

    impl DockerProbe {
        /// Only a definitive `Absent` may hard-fail CI. A `Timeout` in CI proceeds and lets
        /// testcontainers be the authority: if Docker is genuinely broken its container start
        /// still fails the test, so coverage is never silently lost — but a probe blip no
        /// longer turns unrelated PRs red. Locally, anything short of `Available` skips fast
        /// instead of waiting out testcontainers' ~120s container-start retry.
        pub(crate) fn disposition(self, in_ci: bool) -> Disposition {
            match (self, in_ci) {
                (DockerProbe::Available, _) | (DockerProbe::Timeout, true) => Disposition::Proceed,
                (DockerProbe::Absent, true) => Disposition::FailCi,
                (_, false) => Disposition::Skip,
            }
        }
    }

    /// Run `cmd args...` with output discarded and classify the outcome. The child is killed at
    /// the deadline because `docker info` can hang indefinitely when the CLI is installed but
    /// the daemon is down.
    pub(crate) fn probe_command(cmd: &str, args: &[&str], timeout: Duration) -> DockerProbe {
        use std::process::{Command, Stdio};

        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return DockerProbe::Absent; // CLI not installed
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return DockerProbe::Available,
                Ok(Some(_)) => return DockerProbe::Absent,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return DockerProbe::Timeout;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                // try_wait on a child we own essentially never errors; if it does, fail closed
                // (Absent hard-fails CI) rather than risk silently losing coverage.
                Err(_) => return DockerProbe::Absent,
            }
        }
    }

    /// Probe `docker info` once per test binary. testcontainers needs Docker to start the Redis
    /// container; probing up front lets local runs skip fast instead of waiting out its ~120s
    /// container-start retry.
    fn docker_probe() -> DockerProbe {
        static PROBE: std::sync::OnceLock<DockerProbe> = std::sync::OnceLock::new();
        *PROBE.get_or_init(|| probe_command("docker", &["info"], Duration::from_secs(5)))
    }

    /// Start a Redis container and build a store against it. Returns `None` when Docker is
    /// unavailable so the caller can skip — except under `CI`, where a *definitively* absent
    /// Docker is a hard failure (so CI never silently loses Redis coverage). A probe timeout
    /// in CI is not treated as absence; see [`DockerProbe::disposition`].
    async fn setup(ttl: i64) -> Option<(testcontainers::ContainerAsync<Redis>, RedisFlowStore)> {
        let in_ci = std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok();
        match docker_probe().disposition(in_ci) {
            Disposition::Proceed => {}
            Disposition::FailCi => panic!(
                "Docker is required for the Redis integration tests in CI (CI/GITHUB_ACTIONS set) but is not available"
            ),
            Disposition::Skip => {
                eprintln!(
                    "skipping Redis integration test: Docker is not available (start Docker to run these locally)"
                );
                return None;
            }
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

    // ===== per-key ttl, flow-level ttl fix, clear_flow (issue #530) =====

    // Regression for finding 2: flow-level set_ttl was a silent no-op on Redis. It must now actually
    // extend every key's TTL via SCAN + EXPIRE. With a 2s store default, extending to 100s must keep
    // the keys alive past a 3s wait (before the fix they'd expire at 2s and be gone).
    #[tokio::test]
    async fn test_redis_set_ttl_extends_all_keys() {
        let Some((_container, store)) = setup(2).await else {
            return;
        };
        store.set("extendf", "a", json!(1)).unwrap();
        store.set("extendf", "b", json!(2)).unwrap();
        // A sibling flow that is NOT extended must keep its short default TTL and expire.
        store.set("extendsibling", "k", json!(1)).unwrap();

        store.set_ttl("extendf", 100).unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(
            store.exists("extendf", "a").unwrap(),
            "set_ttl must extend key a"
        );
        assert!(
            store.exists("extendf", "b").unwrap(),
            "set_ttl must extend key b"
        );
        assert!(
            !store.exists("extendsibling", "k").unwrap(),
            "set_ttl must extend only the target flow; the sibling keeps its TTL and expires"
        );

        store.clear_flow("extendf").unwrap();
        store.clear_flow("extendsibling").unwrap();
    }

    // Flow-level ttl(<=0) expires every current key (Redis EXPIRE semantics), matching in-memory.
    #[tokio::test]
    async fn test_redis_set_ttl_zero_expires_flow() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };
        store.set("zerof", "a", json!(1)).unwrap();
        store.set("zerof", "b", json!(2)).unwrap();

        store.set_ttl("zerof", 0).unwrap();
        assert!(!store.exists("zerof", "a").unwrap());
        assert!(!store.exists("zerof", "b").unwrap());
    }

    // Per-key ttl mirrors Redis EXPIRE: true when the key exists, false when absent; <=0 deletes.
    #[tokio::test]
    async fn test_redis_set_key_ttl() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };
        store.set("keyttl", "k", json!("v")).unwrap();

        assert!(
            store.set_key_ttl("keyttl", "k", 100).unwrap(),
            "existing key -> true"
        );
        assert!(
            !store.set_key_ttl("keyttl", "absent", 100).unwrap(),
            "absent key -> false"
        );

        // <=0 deletes the key.
        assert!(
            store.set_key_ttl("keyttl", "k", 0).unwrap(),
            "delete existing -> true"
        );
        assert!(
            !store.exists("keyttl", "k").unwrap(),
            "key deleted by ttl<=0"
        );
        assert!(
            !store.set_key_ttl("keyttl", "k", -1).unwrap(),
            "delete absent -> false"
        );
    }

    // clear_flow removes only the target flow's keys.
    #[tokio::test]
    async fn test_redis_clear_flow() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };
        store.set("clearf", "k1", json!(1)).unwrap();
        store.set("clearf", "k2", json!(2)).unwrap();
        store.set("keepf", "k", json!(3)).unwrap();

        store.clear_flow("clearf").unwrap();

        assert!(
            !store.exists("clearf", "k1").unwrap(),
            "target flow cleared"
        );
        assert!(
            !store.exists("clearf", "k2").unwrap(),
            "target flow cleared"
        );
        assert!(
            store.exists("keepf", "k").unwrap(),
            "sibling flow untouched"
        );

        // Idempotent on an absent flow.
        store.clear_flow("no-such-flow").unwrap();
        store.clear_flow("keepf").unwrap();
    }

    // Issue #530 (review finding): a flow_id containing a Redis glob metacharacter must be treated
    // literally by clear_flow's SCAN MATCH — it must NOT wipe sibling flows whose ids the glob would
    // otherwise match. Here clear_flow("a*") must clear only the literal flow "a*", leaving "abc".
    #[tokio::test]
    async fn test_redis_clear_flow_does_not_glob_across_flows() {
        let Some((_container, store)) = setup(300).await else {
            return;
        };
        store.set("a*", "k", json!("literal-star-flow")).unwrap();
        store.set("abc", "k", json!("sibling")).unwrap();

        store.clear_flow("a*").unwrap();

        assert!(
            !store.exists("a*", "k").unwrap(),
            "the literal 'a*' flow must be cleared"
        );
        assert!(
            store.exists("abc", "k").unwrap(),
            "a glob in the flow_id must not clear sibling flows ('abc')"
        );

        // Same guarantee for the destructive flow-level set_ttl(<=0).
        store.set("a*", "k", json!("again")).unwrap();
        store.set_ttl("a*", 0).unwrap();
        assert!(!store.exists("a*", "k").unwrap(), "literal 'a*' expired");
        assert!(
            store.exists("abc", "k").unwrap(),
            "set_ttl(<=0) must not expire sibling flows"
        );

        store.clear_flow("abc").unwrap();
    }

    // Per-key ttl on an already-expired key reports false (the key is absent), on Redis too.
    #[tokio::test]
    async fn test_redis_set_key_ttl_on_expired_key_is_false() {
        let Some((_container, store)) = setup(2).await else {
            return;
        };
        store.set("expttl", "k", json!("v")).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(
            !store.set_key_ttl("expttl", "k", 100).unwrap(),
            "an expired key is absent, so per-key ttl returns false"
        );
    }

    // ===== Docker probe classification (issue #649) — no Docker needed =====
    //
    // The probe must distinguish a *definitive* negative (CLI missing, or a completed
    // `docker info` with non-success exit) from a probe that merely timed out under load.
    // Only the definitive negative may hard-fail CI; a timeout in CI proceeds and lets
    // testcontainers be the authority (issue #649: a 5s `docker info` blip turned
    // unrelated PRs red).

    mod probe_gate {
        use super::{Disposition, DockerProbe, probe_command};
        use std::time::{Duration, Instant};

        #[test]
        fn probe_missing_cli_is_absent() {
            assert_eq!(
                probe_command("rift-no-such-cli-649", &[], Duration::from_secs(5)),
                DockerProbe::Absent
            );
        }

        #[test]
        fn probe_nonzero_exit_is_absent() {
            assert_eq!(
                probe_command("false", &[], Duration::from_secs(5)),
                DockerProbe::Absent
            );
        }

        #[test]
        fn probe_success_is_available() {
            assert_eq!(
                probe_command("true", &[], Duration::from_secs(5)),
                DockerProbe::Available
            );
        }

        #[test]
        fn probe_deadline_exceeded_is_timeout() {
            let start = Instant::now();
            assert_eq!(
                probe_command("sleep", &["30"], Duration::from_millis(200)),
                DockerProbe::Timeout
            );
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "a timed-out probe child must be killed, not waited out"
            );
        }

        #[test]
        fn disposition_available_proceeds() {
            assert_eq!(
                DockerProbe::Available.disposition(true),
                Disposition::Proceed
            );
            assert_eq!(
                DockerProbe::Available.disposition(false),
                Disposition::Proceed
            );
        }

        #[test]
        fn disposition_absent_fails_ci_skips_locally() {
            assert_eq!(DockerProbe::Absent.disposition(true), Disposition::FailCi);
            assert_eq!(DockerProbe::Absent.disposition(false), Disposition::Skip);
        }

        #[test]
        fn disposition_timeout_proceeds_in_ci_skips_locally() {
            assert_eq!(DockerProbe::Timeout.disposition(true), Disposition::Proceed);
            assert_eq!(DockerProbe::Timeout.disposition(false), Disposition::Skip);
        }
    }
}
