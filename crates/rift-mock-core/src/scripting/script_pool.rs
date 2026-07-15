use crate::extensions::flow_state::FlowStore;
use crate::scripting::{FaultDecision, ScriptRequest};
use anyhow::{Result, anyhow};
use crossbeam::channel::{Receiver, Sender, bounded};
use rhai::AST;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

/// Process epoch for the worker deadline clock. `Instant` is not representable as an integer, so
/// deadlines are stored as nanos elapsed since this fixed point (issue #551). u64 nanos covers
/// ~584 years of uptime.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_nanos() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

/// Consult the deadline every 64 Rhai ops, amortizing the `Instant::now()` read (~20-25ns) against
/// Rhai's ~50-200ns/op interpreter overhead — that elides ~98% of clock reads, which is all the
/// amortization is worth.
///
/// The mask sets a floor on enforcement: a script that never reaches the next multiple is never
/// checked. That window must stay small enough not to contain an I/O loop, because the flow-store
/// natives call the *synchronous* redis backend inline on this thread — a handful of slow `get`s is
/// only a few hundred ops, but can hold the worker far past its deadline. A wider mask (1024) makes
/// such a script invisible to the deadline entirely, which strands the worker and sheds load
/// through the queue (the liberation property issue #541 pins).
const DEADLINE_CHECK_MASK: u64 = 0x3F;

/// Deadline slot value meaning "no deadline armed".
const NO_DEADLINE: u64 = u64::MAX;

/// Configuration for the script thread pool
#[derive(Clone, Debug)]

pub struct ScriptPoolConfig {
    /// Number of worker threads. 0 means auto-detect (num_cpus / 2)
    pub workers: usize,
    /// Maximum queue size for pending tasks
    pub queue_size: usize,
    /// Timeout in milliseconds for script execution
    pub timeout_ms: u64,
}

impl Default for ScriptPoolConfig {
    fn default() -> Self {
        let workers = (num_cpus::get() / 2).clamp(2, 16); // Min 2, max 16

        Self {
            workers,
            queue_size: 1000,
            timeout_ms: 5000,
        }
    }
}

/// Compiled script representation for efficient execution
#[derive(Clone)]

pub enum CompiledScript {
    Rhai {
        ast: Arc<AST>,
        rule_id: String,
    },
    #[cfg(feature = "javascript")]
    JavaScript {
        /// JavaScript "bytecode" (currently source since Boa doesn't support serialized bytecode)
        bytecode: Arc<Vec<u8>>,
        rule_id: String,
    },
}

/// A task submitted to the script pool for execution
pub struct ScriptTask {
    pub engine: CompiledScript,
    pub request: ScriptRequest,
    pub flow_store: Arc<dyn FlowStore>,
    pub timeout: Duration,
    pub result_tx: oneshot::Sender<Result<FaultDecision>>,
}

/// Decrements the `queue_depth` and `active_tasks` gauges on drop, so every exit path of
/// `ScriptPool::execute` — success, timeout, or cancel — releases them exactly once (issue #541).
struct CounterGuard<'a> {
    queue_depth: &'a AtomicUsize,
    active_tasks: &'a AtomicUsize,
}

impl Drop for CounterGuard<'_> {
    fn drop(&mut self) {
        self.queue_depth.fetch_sub(1, Ordering::Relaxed);
        self.active_tasks.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Script worker thread
struct ScriptWorker {
    worker_id: usize,
    thread_handle: Option<JoinHandle<()>>,
}

impl ScriptWorker {
    fn spawn(worker_id: usize, work_rx: Receiver<ScriptTask>, shutdown_rx: Receiver<()>) -> Self {
        let handle = thread::Builder::new()
            .name(format!("script-worker-{worker_id}"))
            .spawn(move || {
                debug!("Script worker {} started", worker_id);

                // Per-worker deadline for the task in flight, as nanos since EPOCH, or NO_DEADLINE
                // (issue #551). This replaces a watchdog thread spawned and joined per execution.
                //
                // Only this worker ever writes the slot, and it writes it before starting a task
                // and clears it after — so a deadline can never leak onto a later task. That is
                // what made the old join necessary: a watchdog whose `store(true)` landed late
                // could poison the *next* task's abort flag. The race is designed away rather than
                // re-mitigated, which is also why `Relaxed` is sound here — keep it single-writer.
                let deadline = Arc::new(AtomicU64::new(NO_DEADLINE));

                // Create reusable engine instances per worker with custom functions.
                // `mut` is required to install the on_progress callback.
                let mut rhai_engine = crate::scripting::rhai_engine::RhaiEngine::create_engine();

                // Rhai calls this periodically during AST evaluation; returning Some(_) terminates
                // execution with EvalAltResult::ErrorTerminated. The script now clocks itself
                // against its own deadline instead of waiting to be told by another thread.
                let progress_deadline = Arc::clone(&deadline);
                rhai_engine.on_progress(move |ops| {
                    (ops & DEADLINE_CHECK_MASK == 0
                        && now_nanos() > progress_deadline.load(Ordering::Relaxed))
                    .then_some(rhai::Dynamic::TRUE)
                });

                loop {
                    // Check for shutdown signal (non-blocking)
                    if shutdown_rx.try_recv().is_ok() {
                        debug!("Script worker {} received shutdown signal", worker_id);
                        break;
                    }

                    // Wait for work with timeout to allow shutdown checks
                    match work_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(task) => {
                            let start = Instant::now();
                            let timeout = task.timeout;

                            // Reset the last-flow-error slot: pool workers are long-lived and
                            // reused across requests, so without this a script could observe a
                            // previous request's flow_store error via last_error() (issue #322).
                            crate::extensions::flow_state::clear_last_flow_error();

                            // Arm the deadline for this task. The clock starts at dequeue, matching
                            // the watchdog's semantics. Only Rhai is interruptible: Boa 0.20 has no
                            // per-instruction interrupt, so the JS arm is bounded by RuntimeLimits'
                            // loop-iteration cap plus `execute()`'s outer tokio timeout instead —
                            // arming a deadline it cannot observe would be theatre.
                            let armed = match &task.engine {
                                CompiledScript::Rhai { .. } => {
                                    // Saturate the cast, not just the add: `timeout_ms` is
                                    // unvalidated config, and a bare `as u64` would wrap an absurd
                                    // value down to a *shorter* deadline than a normal one.
                                    // Saturating lands on NO_DEADLINE, which is what an unbounded
                                    // timeout should mean.
                                    let ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
                                    now_nanos().saturating_add(ns)
                                }
                                #[cfg(feature = "javascript")]
                                CompiledScript::JavaScript { .. } => NO_DEADLINE,
                            };
                            deadline.store(armed, Ordering::Relaxed);

                            let result = match &task.engine {
                                CompiledScript::Rhai { ast, rule_id } => Self::execute_rhai(
                                    &rhai_engine,
                                    ast,
                                    &task.request,
                                    task.flow_store.clone(),
                                    rule_id,
                                ),
                                #[cfg(feature = "javascript")]
                                CompiledScript::JavaScript { bytecode, rule_id } => {
                                    Self::execute_javascript(
                                        bytecode,
                                        &task.request,
                                        task.flow_store.clone(),
                                        rule_id,
                                    )
                                }
                            };

                            // Disarm: nothing is running, so no deadline should be live. The task
                            // that follows overwrites this slot before it starts regardless, so
                            // this cannot leak into it.
                            deadline.store(NO_DEADLINE, Ordering::Relaxed);

                            let duration = start.elapsed();
                            if duration >= timeout {
                                warn!(
                                    worker_id,
                                    ?duration,
                                    "Script execution exceeded timeout; Rhai was interrupted"
                                );
                            } else {
                                debug!(worker_id, ?duration, "Script execution completed");
                            }

                            // Send result back (ignore if receiver dropped)
                            let _ = task.result_tx.send(result);
                        }
                        Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                            // Normal timeout, check for shutdown and continue
                            continue;
                        }
                        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                            debug!("Script worker {} channel disconnected", worker_id);
                            break;
                        }
                    }
                }

                debug!("Script worker {} shutting down", worker_id);
            })
            .expect("Failed to spawn script worker thread");

        Self {
            worker_id,
            thread_handle: Some(handle),
        }
    }

    fn execute_rhai(
        engine: &rhai::Engine,
        ast: &Arc<AST>,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        rule_id: &str,
    ) -> Result<FaultDecision> {
        // Import necessary types from rhai_engine module
        use crate::scripting::rhai_engine::execute_rhai_with_engine;

        execute_rhai_with_engine(engine, ast, request, flow_store, rule_id)
    }

    #[cfg(feature = "javascript")]
    fn execute_javascript(
        bytecode: &Arc<Vec<u8>>,
        request: &ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
        rule_id: &str,
    ) -> Result<FaultDecision> {
        // Import necessary function from js_engine module
        use crate::scripting::js_engine::execute_js_bytecode;

        execute_js_bytecode(bytecode.as_slice(), request, flow_store, rule_id)
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            debug!("Waiting for script worker {} to finish", self.worker_id);
            let _ = handle.join();
        }
    }
}

/// Script execution thread pool
pub struct ScriptPool {
    workers: Vec<ScriptWorker>,

    work_tx: Sender<ScriptTask>,
    shutdown_tx: Sender<()>,
    config: ScriptPoolConfig,

    // Metrics
    queue_depth: Arc<AtomicUsize>,

    active_tasks: Arc<AtomicUsize>,
}

impl ScriptPool {
    /// Create a new script pool with the given configuration
    pub fn new(config: ScriptPoolConfig) -> Result<Self> {
        info!(
            "Creating script pool with {} workers, queue size {}",
            config.workers, config.queue_size
        );

        // Create bounded channels
        let (work_tx, work_rx) = bounded(config.queue_size);
        let (shutdown_tx, shutdown_rx) = bounded(config.workers);

        // Spawn workers
        let mut workers = Vec::with_capacity(config.workers);
        for worker_id in 0..config.workers {
            let worker = ScriptWorker::spawn(worker_id, work_rx.clone(), shutdown_rx.clone());
            workers.push(worker);
        }

        Ok(Self {
            workers,
            work_tx,
            shutdown_tx,
            config,
            queue_depth: Arc::new(AtomicUsize::new(0)),
            active_tasks: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Execute a script task asynchronously
    pub async fn execute(
        &self,
        engine: CompiledScript,
        request: ScriptRequest,
        flow_store: Arc<dyn FlowStore>,
    ) -> Result<FaultDecision> {
        let (result_tx, result_rx) = oneshot::channel();
        let timeout = Duration::from_millis(self.config.timeout_ms);

        let task = ScriptTask {
            engine,
            request,
            flow_store,
            timeout,
            result_tx,
        };

        // Track queue depth
        self.queue_depth.fetch_add(1, Ordering::Relaxed);

        // Try to send task to queue
        self.work_tx.try_send(task).map_err(|e| {
            self.queue_depth.fetch_sub(1, Ordering::Relaxed);
            match e {
                crossbeam::channel::TrySendError::Full(_) => {
                    warn!("Script pool queue is full");
                    anyhow!("Script pool queue full")
                }
                crossbeam::channel::TrySendError::Disconnected(_) => {
                    error!("Script pool is shut down");
                    anyhow!("Script pool shut down")
                }
            }
        })?;

        // Track active tasks
        self.active_tasks.fetch_add(1, Ordering::Relaxed);

        // Release both counters on every exit path — success, timeout, or cancel. The `?` on the
        // timeout/cancel branches below returns early, so a manual decrement after them would be
        // skipped and leak the counters permanently (issue #541). Armed here, after the successful
        // `try_send` (whose own error path already decrements queue_depth), so no double-decrement.
        let _counters = CounterGuard {
            queue_depth: &self.queue_depth,
            active_tasks: &self.active_tasks,
        };

        // Wait for result with timeout
        tokio::time::timeout(timeout, result_rx)
            .await
            .map_err(|_| anyhow!("Script execution timed out"))?
            .map_err(|_| anyhow!("Script execution cancelled"))?
    }

    /// Get current queue depth (for metrics)
    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Get number of active tasks (for metrics)
    pub fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::Relaxed)
    }

    /// Get number of workers
    pub fn worker_count(&self) -> usize {
        self.config.workers
    }

    /// Gracefully shutdown the pool
    pub fn shutdown(&mut self) {
        info!(
            "Shutting down script pool with {} workers",
            self.workers.len()
        );

        // Send shutdown signal to all workers
        for _ in 0..self.config.workers {
            let _ = self.shutdown_tx.send(());
        }

        // Wait for all workers to finish
        for worker in &mut self.workers {
            worker.shutdown();
        }

        info!("Script pool shutdown complete");
    }
}

impl Drop for ScriptPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::ResponseMode;

    #[test]
    fn test_pool_creation() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config.clone()).unwrap();
        assert_eq!(pool.worker_count(), 2);
        assert_eq!(pool.queue_depth(), 0);
        assert_eq!(pool.active_tasks(), 0);
    }

    #[test]
    fn test_pool_shutdown() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 5000,
        };

        let mut pool = ScriptPool::new(config).unwrap();
        pool.shutdown();

        // After shutdown, workers should be empty
        assert_eq!(pool.workers.len(), 2); // Workers still in vec but shutdown
    }

    #[test]
    fn test_default_config() {
        let config = ScriptPoolConfig::default();

        // Should be at least 2, at most 16
        assert!(config.workers >= 2);
        assert!(config.workers <= 16);
        assert_eq!(config.queue_size, 1000);
        assert_eq!(config.timeout_ms, 5000);
    }

    // ============================================
    // Additional tests for expanded coverage
    // ============================================

    #[test]
    fn test_pool_config_custom_values() {
        let config = ScriptPoolConfig {
            workers: 8,
            queue_size: 500,
            timeout_ms: 10000,
        };

        assert_eq!(config.workers, 8);
        assert_eq!(config.queue_size, 500);
        assert_eq!(config.timeout_ms, 10000);
    }

    #[test]
    fn test_pool_config_clone() {
        let config = ScriptPoolConfig {
            workers: 4,
            queue_size: 200,
            timeout_ms: 3000,
        };

        let cloned = config.clone();
        assert_eq!(cloned.workers, 4);
        assert_eq!(cloned.queue_size, 200);
        assert_eq!(cloned.timeout_ms, 3000);
    }

    #[test]
    fn test_pool_config_debug() {
        let config = ScriptPoolConfig {
            workers: 4,
            queue_size: 100,
            timeout_ms: 5000,
        };

        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("workers"));
        assert!(debug_str.contains("queue_size"));
        assert!(debug_str.contains("timeout_ms"));
    }

    #[test]
    fn test_pool_single_worker() {
        let config = ScriptPoolConfig {
            workers: 1,
            queue_size: 10,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config).unwrap();
        assert_eq!(pool.worker_count(), 1);
    }

    #[test]
    fn test_pool_many_workers() {
        let config = ScriptPoolConfig {
            workers: 16,
            queue_size: 100,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config).unwrap();
        assert_eq!(pool.worker_count(), 16);
    }

    #[test]
    fn test_pool_small_queue() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 1,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config).unwrap();
        assert_eq!(pool.queue_depth(), 0);
    }

    #[test]
    fn test_pool_metrics_initial() {
        let config = ScriptPoolConfig {
            workers: 4,
            queue_size: 50,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config).unwrap();

        // Initial metrics should be zero
        assert_eq!(pool.queue_depth(), 0);
        assert_eq!(pool.active_tasks(), 0);
        assert_eq!(pool.worker_count(), 4);
    }

    #[test]
    fn test_pool_double_shutdown() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 5000,
        };

        let mut pool = ScriptPool::new(config).unwrap();
        pool.shutdown();
        // Second shutdown should be safe (no-op)
        pool.shutdown();
    }

    #[test]
    fn test_pool_drop() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 5000,
        };

        // Pool should be dropped gracefully
        let _pool = ScriptPool::new(config).unwrap();
        // Drop happens here
    }

    #[test]
    fn test_compiled_script_rhai_creation() {
        use rhai::Engine;

        let engine = Engine::new();
        let ast = engine.compile("fn test() { 42 }").unwrap();

        let compiled = CompiledScript::Rhai {
            ast: Arc::new(ast),
            rule_id: "test-rule".to_string(),
        };

        // `CompiledScript` has a second (`Js`) variant only under the `javascript` feature, so this
        // pattern is refutable there but irrefutable without it (issue #599).
        #[cfg_attr(not(feature = "javascript"), allow(irrefutable_let_patterns))]
        if let CompiledScript::Rhai { rule_id, .. } = compiled {
            assert_eq!(rule_id, "test-rule");
        }
    }

    #[test]
    fn test_compiled_script_clone() {
        use rhai::Engine;

        let engine = Engine::new();
        let ast = engine.compile("fn test() { 42 }").unwrap();

        let compiled = CompiledScript::Rhai {
            ast: Arc::new(ast),
            rule_id: "clone-test".to_string(),
        };

        let cloned = compiled.clone();
        #[cfg_attr(not(feature = "javascript"), allow(irrefutable_let_patterns))]
        if let CompiledScript::Rhai { rule_id, .. } = cloned {
            assert_eq!(rule_id, "clone-test");
        }
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_compiled_script_javascript_creation() {
        let compiled = CompiledScript::JavaScript {
            bytecode: Arc::new(b"function test() {}".to_vec()),
            rule_id: "js-rule".to_string(),
        };

        match compiled {
            CompiledScript::JavaScript { rule_id, bytecode } => {
                assert_eq!(rule_id, "js-rule");
                assert!(!bytecode.is_empty());
            }
            _ => panic!("Expected JavaScript variant"),
        }
    }

    #[test]
    fn test_script_pool_config_minimum_workers() {
        // Default should clamp to at least 2 workers
        let config = ScriptPoolConfig::default();
        assert!(config.workers >= 2, "Default workers should be at least 2");
    }

    #[test]
    fn test_script_pool_config_maximum_workers() {
        // Default should clamp to at most 16 workers
        let config = ScriptPoolConfig::default();
        assert!(config.workers <= 16, "Default workers should be at most 16");
    }

    #[test]
    fn test_pool_timeout_configuration() {
        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 100, // Very short timeout
        };

        let pool = ScriptPool::new(config).unwrap();
        // Pool should be created even with very short timeout
        assert_eq!(pool.worker_count(), 2);
    }

    // =========================================================================
    // Issue #172: script worker must not be permanently blocked by a runaway script
    // =========================================================================

    #[tokio::test]
    async fn test_rhai_infinite_loop_is_interrupted_by_timeout() {
        // An infinite-loop Rhai script must be interrupted when the timeout fires.
        // After the timeout, the same worker must be able to handle a subsequent
        // script — proving it was never permanently blocked.
        use crate::extensions::flow_state::NoOpFlowStore;
        use crate::scripting::ScriptRequest;
        use std::collections::HashMap;

        let config = ScriptPoolConfig {
            workers: 1, // single worker so we can verify it's freed
            queue_size: 10,
            timeout_ms: 100, // very short; infinite loop fires timeout quickly
        };

        let pool = ScriptPool::new(config).unwrap();

        let make_request = || ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let flow_store =
            || -> Arc<dyn crate::extensions::flow_state::FlowStore> { Arc::new(NoOpFlowStore) };

        // Compile an infinite loop script using a Rhai engine without custom functions
        // (on_progress hooks are installed on the *worker* engine; the compile engine
        //  only needs to produce a valid AST). A bare expression (issue #357 Item 2, no
        // `respond` wrapper needed) so the loop actually executes as the entrypoint.
        let compile_engine = rhai::Engine::new();
        let ast = compile_engine
            .compile(
                r#"
                let i = 0;
                loop { i += 1; }
            "#,
            )
            .expect("infinite loop script should compile");

        let compiled = CompiledScript::Rhai {
            ast: Arc::new(ast),
            rule_id: "infinite-loop".to_string(),
        };

        let result = pool.execute(compiled, make_request(), flow_store()).await;

        assert!(
            result.is_err(),
            "Infinite loop must return an error (either tokio timeout or Rhai interrupt)"
        );

        // Give the worker time to finish its internal cleanup after the interrupt: it disarms the
        // deadline and loops back to recv_timeout — well within 300 ms.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Re-use the *same* pool (same single worker) to prove the worker is free.
        // A fresh pool would always pass; only the original pool verifies the fix.
        let compile_engine2 = rhai::Engine::new();
        let ast2 = compile_engine2
            .compile(
                r#"
                fn respond(ctx) {
                    pass()
                }
            "#,
            )
            .expect("normal script should compile");

        let compiled2 = CompiledScript::Rhai {
            ast: Arc::new(ast2),
            rule_id: "post-timeout".to_string(),
        };

        // Raise the pool's timeout for this follow-up call so the normal script
        // doesn't race against the 100 ms limit.
        let result2 = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pool.execute(compiled2, make_request(), flow_store()),
        )
        .await
        .expect("second execute timed out — worker was not freed after interrupt");

        assert!(
            result2.is_ok(),
            "Same worker must handle scripts normally after a timeout interrupt; got: {result2:?}"
        );
    }

    /// A task must never inherit the previous task's deadline (issue #551).
    ///
    /// This pins the property the deleted watchdog `join` used to protect: a watchdog whose
    /// `store(true)` landed late could poison the *next* task's abort flag, so the worker had to
    /// join it before clearing. The deadline slot is single-writer — the owning worker arms it at
    /// dequeue — so the hazard is designed away; this test keeps it that way.
    ///
    /// Task A is interrupted at its deadline, so that deadline is definitely in the past; task B,
    /// dequeued onto the same worker immediately after, must get a fresh one.
    ///
    /// B must be long enough to actually be progress-checked: `on_progress` only consults the
    /// deadline every `DEADLINE_CHECK_MASK + 1` ops, so a trivial script is never checked and
    /// could not detect a stale deadline no matter how broken the slot was.
    #[tokio::test]
    async fn deadline_does_not_leak_from_one_task_to_the_next() {
        use crate::extensions::flow_state::NoOpFlowStore;
        use crate::scripting::ScriptRequest;
        use std::collections::HashMap;

        let pool = ScriptPool::new(ScriptPoolConfig {
            workers: 1, // one worker, so B is guaranteed to reuse A's deadline slot
            queue_size: 10,
            timeout_ms: 500,
        })
        .unwrap();

        let make_request = || ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let flow_store =
            || -> Arc<dyn crate::extensions::flow_state::FlowStore> { Arc::new(NoOpFlowStore) };

        let compile = |src: &str| {
            Arc::new(
                rhai::Engine::new()
                    .compile(src)
                    .expect("script should compile"),
            )
        };

        // Task A: runs until its deadline interrupts it, leaving that deadline in the past.
        let interrupted = pool
            .execute(
                CompiledScript::Rhai {
                    ast: compile("let i = 0; loop { i += 1; }"),
                    rule_id: "leak-source".to_string(),
                },
                make_request(),
                flow_store(),
            )
            .await;
        assert!(interrupted.is_err(), "task A must hit its deadline");

        // Let the worker disarm and return to the recv loop.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Task B on the same worker: enough ops to be progress-checked several times over, but
        // trivially inside a fresh 500 ms budget. If A's expired deadline lingered in the slot,
        // B's first progress check would terminate it.
        let after = pool
            .execute(
                CompiledScript::Rhai {
                    ast: compile("let i = 0; while i < 50000 { i += 1; }"),
                    rule_id: "leak-victim".to_string(),
                },
                make_request(),
                flow_store(),
            )
            .await;

        assert!(
            after.is_ok(),
            "task B must get a fresh deadline, not inherit task A's expired one; got: {after:?}"
        );
    }

    // Issue #541: the timeout/cancel early-return path must release BOTH the queue_depth and
    // active_tasks counters. Before the fix they were decremented only on the success path, so
    // every timed-out script permanently leaked them.
    #[tokio::test]
    async fn execute_timeout_releases_queue_and_active_counters() {
        use crate::extensions::flow_state::NoOpFlowStore;
        use crate::scripting::ScriptRequest;
        use std::collections::HashMap;

        let pool = ScriptPool::new(ScriptPoolConfig {
            workers: 1,
            queue_size: 10,
            timeout_ms: 100,
        })
        .unwrap();

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };
        let flow_store: Arc<dyn crate::extensions::flow_state::FlowStore> = Arc::new(NoOpFlowStore);
        let ast = rhai::Engine::new()
            .compile("let i = 0; loop { i += 1; }")
            .expect("infinite loop compiles");
        let compiled = CompiledScript::Rhai {
            ast: Arc::new(ast),
            rule_id: "leak-check".to_string(),
        };

        let result = pool.execute(compiled, request, flow_store).await;
        assert!(
            result.is_err(),
            "a script exceeding timeout_ms returns an error"
        );

        assert_eq!(
            pool.active_tasks(),
            0,
            "active_tasks must be released after a timed-out script"
        );
        assert_eq!(
            pool.queue_depth(),
            0,
            "queue_depth must be released after a timed-out script"
        );
    }

    #[tokio::test]
    async fn test_pool_execute_simple_rhai_script() {
        use crate::extensions::flow_state::NoOpFlowStore;
        use crate::scripting::ScriptRequest;
        use std::collections::HashMap;

        let config = ScriptPoolConfig {
            workers: 2,
            queue_size: 10,
            timeout_ms: 5000,
        };

        let pool = ScriptPool::new(config).unwrap();

        // Create a simple Rhai script that returns no fault
        let engine = rhai::Engine::new();
        let ast = engine
            .compile(
                r#"
            fn respond(ctx) {
                pass()
            }
        "#,
            )
            .unwrap();

        let compiled = CompiledScript::Rhai {
            ast: Arc::new(ast),
            rule_id: "test".to_string(),
        };

        let request = ScriptRequest {
            mode: ResponseMode::Text,
            raw_body: None,
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: HashMap::new(),
            body: serde_json::json!(null),
            query: HashMap::new(),
            path_params: HashMap::new(),
        };

        let flow_store: Arc<dyn crate::extensions::flow_state::FlowStore> = Arc::new(NoOpFlowStore);

        let result = pool.execute(compiled, request, flow_store).await;
        assert!(result.is_ok());
        // The success path releases both gauges too (issue #541: it now goes through the same
        // CounterGuard::drop as the timeout path).
        assert_eq!(
            pool.active_tasks(),
            0,
            "active_tasks released after success"
        );
        assert_eq!(pool.queue_depth(), 0, "queue_depth released after success");
    }
}
