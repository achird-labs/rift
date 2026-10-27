//! The job loop and the timer globals of the Mountebank-style JS hooks (issue #1593).
//!
//! Mountebank runs a script in Node, so a script may `await`, chain `.then`, or call its callback
//! from a `setTimeout`. Rift runs it in Boa, whose promise reactions and timers are jobs queued on
//! the context's [`JobExecutor`]. [`MbJobExecutor`] is that executor for every MB pool realm: it
//! holds the queued jobs and runs them only through [`MbJobExecutor::drain`], which a hook calls
//! after its function returned, with the run's wall-clock deadline (issue #476). Nothing runs a
//! job outside a drain, so an endless chain of jobs (`async function f() { await 0; f(); }`) ends
//! at the deadline, and a job never outlives the run that queued it: the realm discards what is
//! left before its next run (issue #1645).

use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsValue,
    job::{GenericJob, Job, JobExecutor, NativeAsyncJob, PromiseJob, TimeoutJob},
    js_string,
    native_function::NativeFunction,
    object::builtins::JsFunction,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// One hook run's identity (issues #1611, #1678): the jobs and timers it queues carry it, and
/// its `require` answers only while it is the run being executed. Process-wide, so no two runs
/// ever share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RunId(u64);

impl RunId {
    pub(crate) fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// The number, for a native function's capture (which must be GC-traceable).
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

thread_local! {
    /// The run being executed on this thread, as [`MbJobExecutor::set_current`] last set it: what
    /// the timer functions record as a timer's owner, so the timers a run leaves are forgotten
    /// with it ([`TimerGlobals::forget_run`]).
    static CURRENT_RUN: Cell<Option<RunId>> = const { Cell::new(None) };
}

/// How a [`MbJobExecutor::drain`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drained {
    /// The caller's condition holds.
    Finished,
    /// No job is left, and the condition does not hold.
    Idle,
    /// The deadline passed, or the next timer is due after it.
    Deadline,
}

/// A job ready to run now, with the run that queued it ([`MbJobExecutor::pop_ready`]).
pub(crate) enum ReadyJob {
    Promise(PromiseJob),
    Generic(GenericJob),
    Async(NativeAsyncJob),
    Timer(TimeoutJob),
}

impl ReadyJob {
    /// Run the job. An async job (a dynamic `import()`, which the sealed module loader refuses at
    /// once) is polled once: one still pending is its run's error, as nothing would wake it.
    pub(crate) fn run(self, context: &mut Context) -> JsResult<()> {
        match self {
            Self::Promise(job) => job.call(context).map(drop),
            Self::Generic(job) => job.call(context).map(drop),
            Self::Timer(job) => job.call(context).map(drop),
            Self::Async(job) => {
                let context = RefCell::new(context);
                let mut run = std::pin::pin!(job.call(&context));
                let mut poll = std::task::Context::from_waker(std::task::Waker::noop());
                match run.as_mut().poll(&mut poll) {
                    std::task::Poll::Ready(result) => result.map(drop),
                    std::task::Poll::Pending => Err(JsNativeError::error()
                        .with_message("an asynchronous script job did not settle")
                        .into()),
                }
            }
        }
    }
}

/// What one run has queued ([`MbJobExecutor::queued_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunJobs {
    /// Promise, generic and async jobs, which are ready now.
    pub(crate) ready: usize,
    /// When its first timer is due.
    pub(crate) next_timer: Option<Instant>,
}

/// The job executor of an MB pool realm (issues #1593, #1678). FIFO promise, generic and async
/// jobs, and timers ordered by due time, each tagged with the [`RunId`] that was current when it
/// was queued, so the runs of one realm can interleave and a run's jobs end with it
/// ([`drop_run`](Self::drop_run)): a job never outlives the run that queued it (issue #1645). A
/// job queued while no run is current has no run to belong to and is dropped.
///
/// `Atomics.waitAsync`, whose waiter would outlive its run in Boa's process-wide futex list, is
/// removed from every script context (`js_engine::build_sealed_js_context`). `FinalizationRegistry`
/// cleanups are dropped when queued: the spec lets a host skip them, and they would otherwise wait
/// for a collection that may never come.
#[derive(Default)]
pub(crate) struct MbJobExecutor {
    current: Cell<Option<RunId>>,
    promise_jobs: RefCell<VecDeque<(RunId, PromiseJob)>>,
    generic_jobs: RefCell<VecDeque<(RunId, GenericJob)>>,
    async_jobs: RefCell<VecDeque<(RunId, NativeAsyncJob)>>,
    /// Keyed by due time, then by enqueue order, so timers due together run in the order set.
    timers: RefCell<BTreeMap<(Instant, u64), (RunId, TimeoutJob)>>,
    next_timer: Cell<u64>,
}

impl std::fmt::Debug for MbJobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MbJobExecutor").finish_non_exhaustive()
    }
}

impl MbJobExecutor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Make `run` the run new jobs and timers belong to; the previous one.
    pub(crate) fn set_current(&self, run: Option<RunId>) -> Option<RunId> {
        CURRENT_RUN.set(run);
        self.current.replace(run)
    }

    /// Drop every job and timer `run` queued, without running it.
    pub(crate) fn drop_run(&self, run: RunId) {
        self.promise_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.generic_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.async_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.timers.borrow_mut().retain(|_, (r, _)| *r != run);
    }

    /// The next job to run, if one is ready by `now`: promise and generic jobs first, as Node runs
    /// microtasks before the next timer, then async jobs, then the first due timer.
    pub(crate) fn pop_ready(&self, now: Instant) -> Option<(RunId, ReadyJob)> {
        if let Some((run, job)) = self.promise_jobs.borrow_mut().pop_front() {
            return Some((run, ReadyJob::Promise(job)));
        }
        if let Some((run, job)) = self.generic_jobs.borrow_mut().pop_front() {
            return Some((run, ReadyJob::Generic(job)));
        }
        if let Some((run, job)) = self.async_jobs.borrow_mut().pop_front() {
            return Some((run, ReadyJob::Async(job)));
        }
        let mut timers = self.timers.borrow_mut();
        let due = timers
            .first_key_value()
            .is_some_and(|((due, _), _)| *due <= now);
        if due {
            return timers
                .pop_first()
                .map(|(_, (run, job))| (run, ReadyJob::Timer(job)));
        }
        None
    }

    /// What `run` has queued.
    pub(crate) fn queued_for(&self, run: RunId) -> RunJobs {
        let ready = self
            .promise_jobs
            .borrow()
            .iter()
            .filter(|(r, _)| *r == run)
            .count()
            + self
                .generic_jobs
                .borrow()
                .iter()
                .filter(|(r, _)| *r == run)
                .count()
            + self
                .async_jobs
                .borrow()
                .iter()
                .filter(|(r, _)| *r == run)
                .count();
        let next_timer = self
            .timers
            .borrow()
            .iter()
            .find(|(_, (r, _))| *r == run)
            .map(|((due, _), _)| *due);
        RunJobs { ready, next_timer }
    }

    /// Run `run`'s queued jobs until `finished` holds, it has no job left, or `deadline` passes;
    /// when only timers are left, the thread sleeps until the first is due. A timer due after
    /// `deadline` is not waited for: the run ends with [`Drained::Deadline`] at once. A job that
    /// throws ends the drain with its error, and drops the run's other jobs.
    pub(crate) fn drain(
        &self,
        context: &mut Context,
        run: RunId,
        deadline: Instant,
        mut finished: impl FnMut(&mut Context) -> bool,
    ) -> JsResult<Drained> {
        let outcome = self.drain_inner(context, run, deadline, &mut finished);
        if outcome.is_err() {
            self.drop_run(run);
        }
        context.clear_kept_objects();
        outcome
    }

    fn drain_inner(
        &self,
        context: &mut Context,
        run: RunId,
        deadline: Instant,
        finished: &mut impl FnMut(&mut Context) -> bool,
    ) -> JsResult<Drained> {
        loop {
            if finished(context) {
                return Ok(Drained::Finished);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Drained::Deadline);
            }
            if let Some((_, job)) = self.pop_ready(now) {
                job.run(context)?;
                continue;
            }
            let queued = self.queued_for(run);
            let Some(due) = queued.next_timer else {
                return Ok(Drained::Idle);
            };
            if due > deadline {
                return Ok(Drained::Deadline);
            }
            std::thread::sleep(due.saturating_duration_since(now));
        }
    }
}

impl JobExecutor for MbJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        let Some(run) = self.current.get() else {
            tracing::debug!(
                target: "rift::script",
                "a script job was queued outside any run and is dropped: {job:?}"
            );
            return;
        };
        match job {
            Job::PromiseJob(job) => self.promise_jobs.borrow_mut().push_back((run, job)),
            Job::GenericJob(job) => self.generic_jobs.borrow_mut().push_back((run, job)),
            Job::AsyncJob(job) => self.async_jobs.borrow_mut().push_back((run, job)),
            Job::TimeoutJob(job) => {
                let due = Instant::now() + Duration::from_millis(job.timeout().as_millis());
                let seq = self.next_timer.get();
                self.next_timer.set(seq.wrapping_add(1));
                self.timers.borrow_mut().insert((due, seq), (run, job));
            }
            // Dropped by design: a `FinalizationRegistry` cleanup (see the type's doc), or an
            // interval job, which only `boa_runtime`'s timers queue (Rift's re-queue timeouts).
            other => tracing::debug!(
                target: "rift::script",
                "a script queued a job the MB script runtime does not run: {other:?}"
            ),
        }
    }

    /// Runs the current run's queued jobs to the end, within
    /// [`crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS`]. The hooks never call this; it exists for
    /// Boa's API and keeps its bound.
    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        let Some(run) = self.current.get() else {
            return Ok(());
        };
        let deadline =
            Instant::now() + Duration::from_millis(crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS);
        match self.drain(context, run, deadline, |_| false)? {
            Drained::Deadline => Err(JsNativeError::error()
                .with_message("script jobs did not finish within the script timeout")
                .into()),
            Drained::Finished | Drained::Idle => Ok(()),
        }
    }
}

/// The ids of a realm's live timers, each with the raw [`RunId`] of the run that set it (0 for
/// none), and the last id handed out. A cleared or fired timeout is not live; an interval is live
/// until cleared; a run's timers are forgotten when it ends. One table per realm, as Node has one per process:
/// a run may clear a timer of another run of the realm (issue #1678). GC-traced, so it is a plain
/// capture of the timer natives (no `unsafe`).
type TimerTable = boa_engine::gc::Gc<boa_engine::gc::GcRefCell<(u32, HashMap<u32, u64>)>>;

/// Node's delay for `setTimeout`/`setInterval`: any value that is not a number from 1 to
/// 2^31 - 1 is 1 ms (Node's `TIMEOUT_MAX` rule); a fraction is floored.
fn node_timer_delay_ms(n: f64) -> u64 {
    if (1.0..=2_147_483_647.0).contains(&n) {
        // In range by the check above: `as` floors.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = n as u64;
        ms
    } else {
        1
    }
}

/// Queue timer `id` to call `callback(...args)` after `delay_ms`, again every `delay_ms` when
/// `repeat`, for as long as `id` is live in `timers`.
fn schedule_timer(
    context: &mut Context,
    timers: TimerTable,
    id: u32,
    callback: JsFunction,
    args: Vec<JsValue>,
    delay_ms: u64,
    repeat: bool,
) {
    let job = TimeoutJob::new(
        boa_engine::job::NativeJob::new(move |context| {
            if !timers.borrow().1.contains_key(&id) {
                return Ok(JsValue::undefined());
            }
            if !repeat {
                timers.borrow_mut().1.remove(&id);
            }
            callback.call(&JsValue::undefined(), &args, context)?;
            if repeat && timers.borrow().1.contains_key(&id) {
                schedule_timer(context, timers, id, callback, args, delay_ms, repeat);
            }
            Ok(JsValue::undefined())
        }),
        delay_ms,
    );
    context.enqueue_job(job.into());
}

fn set_timer(
    args: &[JsValue],
    timers: &TimerTable,
    context: &mut Context,
    repeat: bool,
) -> JsResult<JsValue> {
    let callback = args
        .first()
        .and_then(JsValue::as_object)
        .and_then(JsFunction::from_object)
        .ok_or_else(|| {
            JsNativeError::typ().with_message("The \"callback\" argument must be of type function")
        })?;
    let delay = match args.get(1) {
        Some(delay) => delay.to_number(context)?,
        None => f64::NAN,
    };
    let id = {
        let mut table = timers.borrow_mut();
        table.0 = table.0.wrapping_add(1);
        let id = table.0;
        let owner = CURRENT_RUN.get().map_or(0, RunId::raw);
        table.1.insert(id, owner);
        id
    };
    let rest = args.get(2..).map(<[JsValue]>::to_vec).unwrap_or_default();
    schedule_timer(
        context,
        timers.clone(),
        id,
        callback,
        rest,
        node_timer_delay_ms(delay),
        repeat,
    );
    Ok(JsValue::from(id))
}

fn clear_timer(args: &[JsValue], timers: &TimerTable, context: &mut Context) -> JsResult<JsValue> {
    // Domain-optional: Node ignores `clearTimeout(undefined)` or any value that is no timer.
    if let Some(id) = args.first().filter(|v| v.is_number()) {
        let id = id.to_number(context)?;
        if (0.0..=f64::from(u32::MAX)).contains(&id) && id.fract() == 0.0 {
            // In range and whole by the check above.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            timers.borrow_mut().1.remove(&(id as u32));
        }
    }
    Ok(JsValue::undefined())
}

type TimerFn = fn(&JsValue, &[JsValue], &TimerTable, &mut Context) -> JsResult<JsValue>;

/// `setTimeout`, `clearTimeout`, `setInterval` and `clearInterval` over one timer table (issue
/// #1593): built once per context by [`install_timers`], and set on the global object again by
/// [`TimerGlobals::install`] at each run start, so a script that overwrote one in a shared realm
/// does not break a later run.
#[derive(Debug, Clone)]
pub(crate) struct TimerGlobals {
    natives: [(&'static str, JsFunction); 4],
    table: TimerTable,
}

impl TimerGlobals {
    pub(crate) fn install(&self, context: &mut Context) -> JsResult<()> {
        let global: JsObject = context.global_object();
        for (name, native) in &self.natives {
            global.set(js_string!(*name), native.clone(), false, context)?;
        }
        Ok(())
    }

    /// The live timers' count.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.table.borrow().1.len()
    }

    /// Forget the timers `run` set: it ended, and its timer jobs were dropped with it.
    pub(crate) fn forget_run(&self, run: RunId) {
        self.table
            .borrow_mut()
            .1
            .retain(|_, owner| *owner != run.raw());
    }
}

/// Build the four timer functions over a fresh timer table and set them on the global object
/// (issue #1593). A timer's callback runs only when the context's [`MbJobExecutor`] runs it; a
/// context with another executor queues it and never runs it.
pub(crate) fn install_timers(context: &mut Context) -> JsResult<TimerGlobals> {
    let timers: TimerTable =
        boa_engine::gc::Gc::new(boa_engine::gc::GcRefCell::new((0, HashMap::new())));
    let natives: [(&'static str, TimerFn); 4] = [
        ("setTimeout", |_, args, timers, context| {
            set_timer(args, timers, context, false)
        }),
        ("setInterval", |_, args, timers, context| {
            set_timer(args, timers, context, true)
        }),
        ("clearTimeout", |_, args, timers, context| {
            clear_timer(args, timers, context)
        }),
        ("clearInterval", |_, args, timers, context| {
            clear_timer(args, timers, context)
        }),
    ];
    let globals = TimerGlobals {
        natives: natives.map(|(name, func)| {
            let native = NativeFunction::from_copy_closure_with_captures(func, timers.clone())
                .to_js_function(context.realm());
            (name, native)
        }),
        table: timers,
    };
    globals.install(context)?;
    Ok(globals)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context on an [`MbJobExecutor`], with one run current.
    fn context() -> (Context, Rc<MbJobExecutor>, RunId) {
        let jobs = Rc::new(MbJobExecutor::new());
        let mut context = crate::scripting::js_engine::sealed_js_context_on(Rc::clone(&jobs));
        install_timers(&mut context).expect("timers");
        let run = RunId::next();
        jobs.set_current(Some(run));
        (context, jobs, run)
    }

    fn global(context: &mut Context, name: &str) -> JsValue {
        context
            .global_object()
            .get(js_string!(name), context)
            .expect("global")
    }

    fn eval(context: &mut Context, source: &str) {
        context
            .eval(boa_engine::Source::from_bytes(source.as_bytes()))
            .expect("eval");
    }

    fn string(context: &mut Context, name: &str) -> String {
        let value = global(context, name);
        value
            .to_string(context)
            .expect("string")
            .to_std_string_escaped()
    }

    /// Run every ready job, as the worker does, until none is left; the runs they belonged to.
    fn run_ready(context: &mut Context, jobs: &MbJobExecutor) -> Vec<RunId> {
        let mut ran = Vec::new();
        while let Some((run, job)) = jobs.pop_ready(Instant::now()) {
            jobs.set_current(Some(run));
            job.run(context).expect("job");
            ran.push(run);
        }
        ran
    }

    #[test]
    fn node_delay_rule() {
        assert_eq!(node_timer_delay_ms(f64::NAN), 1);
        assert_eq!(node_timer_delay_ms(0.0), 1);
        assert_eq!(node_timer_delay_ms(-5.0), 1);
        assert_eq!(node_timer_delay_ms(2_147_483_648.0), 1);
        assert_eq!(node_timer_delay_ms(20.7), 20);
    }

    #[test]
    fn microtasks_run_before_a_due_timer() {
        let (mut context, jobs, run) = context();
        eval(
            &mut context,
            "var order = []; setTimeout(() => order.push('t'), 0); \
             Promise.resolve().then(() => order.push('p'));",
        );
        let end = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        assert_eq!(string(&mut context, "order"), "p,t");
    }

    #[test]
    fn an_interval_runs_until_cleared() {
        let (mut context, jobs, run) = context();
        eval(
            &mut context,
            "var n = 0; var id = setInterval(() => { n++; if (n === 3) clearInterval(id); }, 1);",
        );
        let end = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        assert_eq!(global(&mut context, "n"), JsValue::from(3));
    }

    #[test]
    fn an_endless_microtask_chain_ends_at_the_deadline() {
        let (mut context, jobs, run) = context();
        eval(&mut context, "async function f() { await 0; f(); } f();");
        let started = Instant::now();
        let end = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_millis(50),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Deadline);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_timer_due_after_the_deadline_is_not_waited_for() {
        let (mut context, jobs, run) = context();
        eval(&mut context, "setTimeout(() => {}, 10000);");
        let started = Instant::now();
        let end = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_millis(100),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Deadline);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn a_throwing_timer_ends_the_drain_and_drops_the_rest() {
        let (mut context, jobs, run) = context();
        eval(
            &mut context,
            "var later = false; setTimeout(() => { throw new Error('tick'); }, 0); \
             setTimeout(() => { later = true; }, 1);",
        );
        let err = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect_err("the throw");
        assert!(err.to_string().contains("tick"), "{err}");
        let end = jobs
            .drain(
                &mut context,
                run,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        assert_eq!(global(&mut context, "later"), JsValue::from(false));
    }

    #[test]
    fn set_timeout_needs_a_function() {
        let (mut context, _jobs, _run) = context();
        let err = context
            .eval(boa_engine::Source::from_bytes(b"setTimeout('1 + 1', 1)"))
            .expect_err("a string is no callback");
        assert!(err.to_string().contains("callback"), "{err}");
    }

    // Issue #1678: every job carries the run that was current when it was queued, including the
    // reactions and timers its own jobs queue.
    #[test]
    fn jobs_belong_to_the_run_that_queued_them() {
        let (mut context, jobs, a) = context();
        eval(
            &mut context,
            "Promise.resolve().then(() => { Promise.resolve().then(() => {}); }); \
             setTimeout(() => {}, 0);",
        );
        let b = RunId::next();
        jobs.set_current(Some(b));
        eval(&mut context, "Promise.resolve().then(() => {});");
        assert_eq!(jobs.queued_for(a).ready, 1);
        assert!(jobs.queued_for(a).next_timer.is_some());
        assert_eq!(jobs.queued_for(b).ready, 1);
        std::thread::sleep(Duration::from_millis(2));
        let ran = run_ready(&mut context, &jobs);
        // FIFO across runs: a's reaction, b's reaction, then the reaction a's queued, then a's timer.
        assert_eq!(ran, [a, b, a, a]);
        let none = RunJobs {
            ready: 0,
            next_timer: None,
        };
        assert_eq!((jobs.queued_for(a), jobs.queued_for(b)), (none, none));
    }

    #[test]
    fn drop_run_keeps_the_other_runs_jobs() {
        let (mut context, jobs, a) = context();
        eval(
            &mut context,
            "var fired = []; setTimeout(() => fired.push('a'), 0);",
        );
        let b = RunId::next();
        jobs.set_current(Some(b));
        eval(
            &mut context,
            "setTimeout(() => fired.push('b'), 0); Promise.resolve().then(() => fired.push('bp'));",
        );
        jobs.drop_run(b);
        assert_eq!(
            jobs.queued_for(b),
            RunJobs {
                ready: 0,
                next_timer: None
            }
        );
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(run_ready(&mut context, &jobs), [a]);
        assert_eq!(string(&mut context, "fired"), "a");
    }

    #[test]
    fn a_job_queued_outside_any_run_is_dropped() {
        let (mut context, jobs, _run) = context();
        jobs.set_current(None);
        eval(
            &mut context,
            "Promise.resolve().then(() => {}); setTimeout(() => {}, 0);",
        );
        std::thread::sleep(Duration::from_millis(2));
        assert!(jobs.pop_ready(Instant::now()).is_none());
    }

    // One timer table per realm: a run can clear a timer another run of the realm set, as any
    // code can in Node's one process.
    #[test]
    fn a_run_may_clear_another_runs_timer() {
        let (mut context, jobs, _a) = context();
        eval(
            &mut context,
            "var fired = false; var id = setTimeout(() => { fired = true; }, 0);",
        );
        jobs.set_current(Some(RunId::next()));
        eval(&mut context, "clearTimeout(id);");
        std::thread::sleep(Duration::from_millis(2));
        run_ready(&mut context, &jobs);
        assert_eq!(global(&mut context, "fired"), JsValue::from(false));
    }

    // A realm's timer table lives as long as the realm: the timers a run leaves behind (a timeout
    // that never fired, an interval never cleared) are forgotten when the run ends.
    #[test]
    fn a_runs_timers_are_forgotten_when_it_ends() {
        let jobs = Rc::new(MbJobExecutor::new());
        let mut context = crate::scripting::js_engine::sealed_js_context_on(Rc::clone(&jobs));
        let timers = install_timers(&mut context).expect("timers");
        let a = RunId::next();
        jobs.set_current(Some(a));
        eval(
            &mut context,
            "setTimeout(() => {}, 10000); setInterval(() => {}, 10000);",
        );
        let b = RunId::next();
        jobs.set_current(Some(b));
        eval(&mut context, "setTimeout(() => {}, 10000);");
        assert_eq!(timers.live(), 3);
        timers.forget_run(a);
        assert_eq!(timers.live(), 1);
    }
}
