//! Opt-in runtime topology for the server binary (RFC-712, issue #744).
//!
//! Default is unchanged: one multi-threaded work-stealing tokio runtime. `--runtime per-core[=N]`
//! (or `RIFT_RUNTIME`) selects N single-threaded worker runtimes fed by per-worker command
//! channels, with the control plane (admin API, metrics, imposter mutations) on a small
//! multi-thread runtime. In this issue the workers only answer `Ping` and `Shutdown`; imposter
//! `Bind`/`Unbind` fan-out arrives with the follow-up (#745), so per-core mode is not yet
//! user-visible behavior — only topology plumbing.
//!
//! Platform policy (RFC-712 D5): Linux is first-class (SO_REUSEPORT balances accepts by 4-tuple
//! hash — the design's premise). macOS *refuses* per-core with a warning and falls back to
//! work-stealing: BSD/XNU SO_REUSEPORT does not hash-balance TCP accepts across the group, so a
//! per-core mode there would funnel most connections to one worker — worse than work-stealing.
//! Windows rejects the flag outright (no SO_REUSEPORT semantics).

use std::io;
use tokio::sync::{mpsc, oneshot};

/// How many worker runtimes `per-core` means when no explicit count is given.
fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Per-worker blocking-pool cap (RFC-712 D3): tokio's default 512 per runtime would multiply to
/// N×512 threads across N worker runtimes. The enforced invariant is per-worker ∈ [8, 64]; for
/// the realistic 8–64-worker range that puts the workers' combined ceiling near a single
/// runtime's 512 (outside it, the floor/ceiling wins: fewer workers stay responsive at 64 each,
/// and a >64-worker box accepts workers×8).
pub fn max_blocking_threads(workers: usize) -> usize {
    (512 / workers.max(1)).clamp(8, 64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTopology {
    WorkStealing,
    PerCore { workers: usize },
}

impl RuntimeTopology {
    /// Parse the `--runtime` / `RIFT_RUNTIME` grammar: `work-stealing` | `per-core` |
    /// `per-core=N` (N ≥ 1). Anything else is a hard error — a typo must never silently run the
    /// default topology while the operator believes per-core is active.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "work-stealing" => Ok(Self::WorkStealing),
            "per-core" => Ok(Self::PerCore {
                workers: default_worker_count(),
            }),
            _ => match s.strip_prefix("per-core=") {
                Some(n) => match n.parse::<usize>() {
                    Ok(workers) if workers >= 1 => Ok(Self::PerCore { workers }),
                    _ => Err(format!(
                        "invalid worker count {n:?} in --runtime per-core=N (need an integer >= 1)"
                    )),
                },
                None => Err(format!(
                    "invalid --runtime {s:?}: expected work-stealing, per-core, or per-core=N"
                )),
            },
        }
    }

    /// Resolve from the CLI flag and the `RIFT_RUNTIME` env var; the CLI wins, and absent both
    /// the default is today's work-stealing topology. An empty/whitespace value counts as
    /// unset — `RIFT_RUNTIME=""` from a container's `${VAR}` expansion of an unset variable
    /// must not refuse to boot (empty is absence, not a typo).
    ///
    /// In production clap has already merged the env var into the CLI value; the separate
    /// `env` parameter exists so the precedence rule stays unit-testable.
    pub fn resolve(cli: Option<&str>, env: Option<&str>) -> Result<Self, String> {
        fn pick(v: Option<&str>) -> Option<&str> {
            v.map(str::trim).filter(|s| !s.is_empty())
        }
        match pick(cli).or(pick(env)) {
            Some(s) => Self::parse(s),
            None => Ok(Self::WorkStealing),
        }
    }

    /// A short display form for the startup marker (`Runtime topology: …`), so benchmarks and
    /// operators read the topology off the binary itself — same pattern as `Global allocator:`.
    pub fn describe(&self) -> String {
        match self {
            Self::WorkStealing => "work-stealing".to_string(),
            Self::PerCore { workers } => format!("per-core x{workers}"),
        }
    }
}

/// Operating systems the platform gate distinguishes. A parameter rather than bare `cfg` so every
/// branch of the policy is unit-testable on any development platform; [`current_os`] is the only
/// cfg-dependent piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
    Other,
}

pub fn current_os() -> Os {
    if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Other
    }
}

/// Apply the RFC-712 D5 platform policy. Returns the topology to actually run plus an optional
/// warning to log; `Err` means the combination must refuse to start.
pub fn platform_gate(
    requested: RuntimeTopology,
    os: Os,
) -> Result<(RuntimeTopology, Option<String>), String> {
    match (requested, os) {
        (RuntimeTopology::WorkStealing, _) => Ok((requested, None)),
        (RuntimeTopology::PerCore { .. }, Os::Linux) => Ok((requested, None)),
        (RuntimeTopology::PerCore { .. }, Os::MacOs | Os::Other) => Ok((
            RuntimeTopology::WorkStealing,
            Some(
                "per-core runtime requested, but this platform's SO_REUSEPORT does not \
                 hash-balance TCP accepts; falling back to work-stealing (RFC-712 D5)"
                    .to_string(),
            ),
        )),
        (RuntimeTopology::PerCore { .. }, Os::Windows) => Err(
            "--runtime per-core is not supported on Windows (no SO_REUSEPORT semantics); \
             use work-stealing"
                .to_string(),
        ),
    }
}

/// Commands a worker runtime accepts. `Bind`/`Unbind` for imposter listeners arrive with #745;
/// until then `Ping` doubles as the liveness/identification probe (#746 needs per-worker
/// identification for the accept counters anyway).
#[derive(Debug)]
pub enum WorkerCommand {
    /// Reply with this worker's index — proves the async command loop is live on its runtime.
    Ping(oneshot::Sender<usize>),
    Shutdown,
}

/// N worker OS threads, each owning a current-thread tokio runtime driven by a command channel.
/// Dropping the set without [`WorkerSet::shutdown`] would leak threads, hence `#[must_use]`.
#[must_use = "call shutdown() so worker threads join instead of leaking"]
pub struct WorkerSet {
    senders: Vec<mpsc::UnboundedSender<WorkerCommand>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl WorkerSet {
    /// Spawn `workers` threads. `pin` requests core affinity (RFC-712 D4; effective on Linux,
    /// advisory/no-op elsewhere — pinning failure is logged, never fatal: an unpinned worker is
    /// merely slower, and dying over it would turn a scheduling hint into an outage).
    pub fn spawn(workers: usize, pin: bool) -> io::Result<Self> {
        let core_ids = if pin {
            let ids = core_affinity::get_core_ids().unwrap_or_default();
            if ids.is_empty() {
                // The operator explicitly asked for pinning; a silent no-op would let them
                // believe workers are pinned when none are.
                tracing::warn!(
                    "--runtime-affinity requested but core enumeration failed; workers run unpinned"
                );
            }
            ids
        } else {
            Vec::new()
        };
        let blocking = max_blocking_threads(workers);
        let mut senders = Vec::with_capacity(workers);
        let mut handles = Vec::with_capacity(workers);
        for index in 0..workers {
            let (tx, mut rx) = mpsc::unbounded_channel::<WorkerCommand>();
            let core = core_ids.get(index).copied();
            let handle = std::thread::Builder::new()
                .name(format!("rift-worker-{index}"))
                .spawn(move || {
                    if let Some(core) = core
                        && !core_affinity::set_for_current(core)
                    {
                        tracing::warn!(worker = index, "failed to pin worker to core");
                    }
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .max_blocking_threads(blocking)
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            // The spawner's Ping handshake detects the dead worker; the error
                            // is logged here where the cause is known.
                            tracing::error!(worker = index, "worker runtime build failed: {e}");
                            return;
                        }
                    };
                    runtime.block_on(async move {
                        while let Some(cmd) = rx.recv().await {
                            match cmd {
                                WorkerCommand::Ping(reply) => {
                                    // A dropped receiver just means the caller stopped waiting.
                                    let _ = reply.send(index);
                                }
                                WorkerCommand::Shutdown => break,
                            }
                        }
                    });
                })?;
            senders.push(tx);
            handles.push(handle);
        }
        Ok(Self { senders, handles })
    }

    pub fn worker_count(&self) -> usize {
        self.senders.len()
    }

    /// Ping every worker and collect the indices that answered — the liveness handshake the
    /// bootstrap runs before declaring per-core mode up (a worker whose runtime failed to build
    /// is detected here, not at first traffic).
    pub async fn ping_all(&self) -> Vec<usize> {
        let mut alive = Vec::with_capacity(self.senders.len());
        for tx in &self.senders {
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx.send(WorkerCommand::Ping(reply_tx)).is_ok()
                && let Ok(index) = reply_rx.await
            {
                alive.push(index);
            }
        }
        alive
    }

    /// Broadcast `Shutdown` and join every worker thread. Consumes the set; joining is what
    /// keeps `cargo test` (and the binary's exit path) free of leaked threads.
    pub fn shutdown(self) {
        for tx in &self.senders {
            let _ = tx.send(WorkerCommand::Shutdown);
        }
        for (index, handle) in self.handles.into_iter().enumerate() {
            if let Err(payload) = handle.join() {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                tracing::error!(worker = index, "worker thread panicked: {message}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #744 AC: the --runtime/RIFT_RUNTIME grammar, with typos as hard errors.
    #[test]
    fn parses_work_stealing() {
        assert_eq!(
            RuntimeTopology::parse("work-stealing").unwrap(),
            RuntimeTopology::WorkStealing
        );
    }

    #[test]
    fn parses_per_core_with_default_worker_count() {
        match RuntimeTopology::parse("per-core").unwrap() {
            RuntimeTopology::PerCore { workers } => assert!(workers >= 1),
            other => panic!("expected per-core, got {other:?}"),
        }
    }

    #[test]
    fn parses_explicit_worker_count() {
        assert_eq!(
            RuntimeTopology::parse("per-core=4").unwrap(),
            RuntimeTopology::PerCore { workers: 4 }
        );
    }

    #[test]
    fn rejects_zero_workers_and_garbage() {
        assert!(RuntimeTopology::parse("per-core=0").is_err());
        assert!(RuntimeTopology::parse("per-core=x").is_err());
        assert!(RuntimeTopology::parse("per-core=").is_err());
        assert!(RuntimeTopology::parse("threads").is_err());
        assert!(RuntimeTopology::parse("").is_err());
    }

    #[test]
    fn resolve_cli_beats_env_and_default_is_work_stealing() {
        assert_eq!(
            RuntimeTopology::resolve(Some("per-core=2"), Some("work-stealing")).unwrap(),
            RuntimeTopology::PerCore { workers: 2 }
        );
        assert_eq!(
            RuntimeTopology::resolve(None, Some("per-core=3")).unwrap(),
            RuntimeTopology::PerCore { workers: 3 }
        );
        assert_eq!(
            RuntimeTopology::resolve(None, None).unwrap(),
            RuntimeTopology::WorkStealing
        );
        assert!(RuntimeTopology::resolve(None, Some("bogus")).is_err());
    }

    // Issue #744 AC: platform gates — every OS branch testable on any dev platform.
    #[test]
    fn platform_gate_linux_keeps_per_core() {
        let requested = RuntimeTopology::PerCore { workers: 8 };
        let (effective, warn) = platform_gate(requested, Os::Linux).unwrap();
        assert_eq!(effective, requested);
        assert!(warn.is_none());
    }

    #[test]
    fn platform_gate_macos_falls_back_with_warning() {
        let (effective, warn) =
            platform_gate(RuntimeTopology::PerCore { workers: 8 }, Os::MacOs).unwrap();
        assert_eq!(effective, RuntimeTopology::WorkStealing);
        assert!(warn.unwrap().contains("falling back"));
    }

    #[test]
    fn platform_gate_windows_rejects_per_core() {
        assert!(platform_gate(RuntimeTopology::PerCore { workers: 8 }, Os::Windows).is_err());
    }

    #[test]
    fn platform_gate_never_touches_work_stealing() {
        for os in [Os::Linux, Os::MacOs, Os::Windows, Os::Other] {
            let (effective, warn) = platform_gate(RuntimeTopology::WorkStealing, os).unwrap();
            assert_eq!(effective, RuntimeTopology::WorkStealing);
            assert!(warn.is_none());
        }
    }

    // Issue #744 AC: blocking-pool clamp — global ceiling ~512 preserved, per-worker in [8, 64].
    #[test]
    fn blocking_clamp_bounds() {
        assert_eq!(max_blocking_threads(1), 64);
        assert_eq!(max_blocking_threads(8), 64);
        assert_eq!(max_blocking_threads(16), 32);
        assert_eq!(max_blocking_threads(64), 8);
        assert_eq!(max_blocking_threads(1024), 8);
        assert_eq!(
            max_blocking_threads(0),
            64,
            "degenerate zero never divides by zero"
        );
    }

    // Issue #744 AC: workers spawn with live runtimes and shut down without leaking threads.
    #[test]
    fn worker_set_spawns_pings_and_shuts_down() {
        let set = WorkerSet::spawn(3, false).expect("spawn worker set");
        assert_eq!(set.worker_count(), 3);
        let control = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("control runtime");
        let mut alive = control.block_on(set.ping_all());
        alive.sort_unstable();
        assert_eq!(
            alive,
            vec![0, 1, 2],
            "every worker's command loop answers on its runtime"
        );
        set.shutdown(); // joins — a hung worker would hang the test, which is the point
    }

    // Container idiom `RIFT_RUNTIME=${VAR}` with VAR unset expands to empty — that is absence,
    // not a typo, and must not refuse to boot.
    #[test]
    fn resolve_treats_empty_and_whitespace_as_unset() {
        assert_eq!(
            RuntimeTopology::resolve(None, Some("")).unwrap(),
            RuntimeTopology::WorkStealing
        );
        assert_eq!(
            RuntimeTopology::resolve(Some("  "), None).unwrap(),
            RuntimeTopology::WorkStealing
        );
        assert_eq!(
            RuntimeTopology::resolve(Some(""), Some("per-core=2")).unwrap(),
            RuntimeTopology::PerCore { workers: 2 },
            "an empty CLI value falls through to the env value"
        );
    }

    // The degraded-start refusal in main.rs branches on ping_all reporting a PARTIAL set —
    // prove the partial-alive signal itself: kill one worker, the other two still answer.
    #[test]
    fn ping_all_reports_partial_set_when_a_worker_is_down() {
        let set = WorkerSet::spawn(3, false).expect("spawn worker set");
        let control = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("control runtime");
        set.senders[0]
            .send(WorkerCommand::Shutdown)
            .expect("worker 0 accepts its shutdown");
        // Wait for worker 0's loop to exit: its Ping reply channel drops once the loop breaks.
        control.block_on(async {
            for _ in 0..100 {
                let (tx, rx) = oneshot::channel();
                if set.senders[0].send(WorkerCommand::Ping(tx)).is_err() || rx.await.is_err() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("worker 0 never went down");
        });
        let mut alive = control.block_on(set.ping_all());
        alive.sort_unstable();
        assert_eq!(alive, vec![1, 2], "exactly the live workers answer");
        set.shutdown();
    }

    #[test]
    fn describe_reads_like_the_startup_marker() {
        assert_eq!(RuntimeTopology::WorkStealing.describe(), "work-stealing");
        assert_eq!(
            RuntimeTopology::PerCore { workers: 4 }.describe(),
            "per-core x4"
        );
    }
}
