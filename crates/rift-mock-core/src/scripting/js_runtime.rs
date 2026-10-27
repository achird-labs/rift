//! The job loop and the timer globals of the Mountebank-style JS hooks (issue #1593).
//!
//! Mountebank runs a script in Node, so a script may `await`, chain `.then`, or call its callback
//! from a `setTimeout`. Rift runs it in Boa, whose promise reactions and timers are jobs queued on
//! the context's [`JobExecutor`]. [`MbJobExecutor`] is that executor for every MB pool realm: it
//! holds the queued jobs, each tagged with the run that queued it, and hands them out one at a time
//! to the worker's event loop (`js_engine::MbRealm::step`, issue #1678), which runs each as its own
//! run. A job never outlives the deadline of the run that queued it (issues #476, #1645): a run's
//! jobs end with it, so an endless chain of jobs (`async function f() { await 0; f(); }`) ends at
//! its deadline; when a run answers while another run of its imposter waits in the realm, what it
//! left passes to that run ([`MbJobExecutor::retag`]), still bounded by the first run's deadline,
//! as are the jobs those queue in turn.

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

/// The run executing on this thread ([`CURRENT_RUN`]).
pub(crate) fn current_run() -> Option<RunId> {
    CURRENT_RUN.get()
}

/// Restore the run executing on this thread, as a nested run's guard does on its way out.
pub(crate) fn set_current_run(run: Option<RunId>) {
    CURRENT_RUN.set(run);
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

/// Whom a queued job belongs to: the run it runs as, and, for a job a run inherited from another
/// ([`MbJobExecutor::retag`]), the deadline of the run that first queued it, past which it is
/// dropped. The jobs such a job queues carry the same deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tag {
    pub(crate) run: RunId,
    pub(crate) expiry: Option<Instant>,
}

impl Tag {
    fn live(self, now: Instant) -> bool {
        self.expiry.is_none_or(|expiry| now < expiry)
    }
}

impl PartialEq<RunId> for Tag {
    fn eq(&self, run: &RunId) -> bool {
        self.run == *run
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
/// jobs, and timers ordered by due time, each [tagged](Tag) with the [`RunId`] that was current
/// when it was queued, so the runs of one realm can interleave and a run's jobs end with it
/// ([`drop_run`](Self::drop_run)), or by its deadline when another run inherited them
/// ([`retag`](Self::retag)): a job never outlives the deadline of the run that queued it (issue
/// #1645). A job queued while no run is current has no run to belong to and is dropped.
///
/// `Atomics.waitAsync`, whose waiter would outlive its run in Boa's process-wide futex list, is
/// removed from every script context (`js_engine::build_sealed_js_context`). `FinalizationRegistry`
/// cleanups are dropped when queued: the spec lets a host skip them, and they would otherwise wait
/// for a collection that may never come.
#[derive(Default)]
pub(crate) struct MbJobExecutor {
    current: Cell<Option<RunId>>,
    /// The expiry of the inherited job being run, which the jobs it queues inherit.
    expiry: Cell<Option<Instant>>,
    promise_jobs: RefCell<VecDeque<(Tag, PromiseJob)>>,
    generic_jobs: RefCell<VecDeque<(Tag, GenericJob)>>,
    async_jobs: RefCell<VecDeque<(Tag, NativeAsyncJob)>>,
    /// Keyed by due time, then by enqueue order, so timers due together run in the order set.
    timers: RefCell<BTreeMap<(Instant, u64), (Tag, TimeoutJob)>>,
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

    /// Make `expiry` the deadline of the jobs queued from now on (the expiry of an inherited job
    /// while it runs); the previous one.
    pub(crate) fn set_expiry(&self, expiry: Option<Instant>) -> Option<Instant> {
        self.expiry.replace(expiry)
    }

    /// Drop every job and timer `run` queued, without running it.
    pub(crate) fn drop_run(&self, run: RunId) {
        self.promise_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.generic_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.async_jobs.borrow_mut().retain(|(r, _)| *r != run);
        self.timers.borrow_mut().retain(|_, (r, _)| *r != run);
    }

    /// The next job to run, if one is ready by `now`: promise and generic jobs first, as Node runs
    /// microtasks before the next timer, then async jobs, then the first due timer. A job past its
    /// expiry is dropped on the way.
    pub(crate) fn pop_ready(&self, now: Instant) -> Option<(Tag, ReadyJob)> {
        fn front<J>(jobs: &RefCell<VecDeque<(Tag, J)>>, now: Instant) -> Option<(Tag, J)> {
            let mut jobs = jobs.borrow_mut();
            while let Some((tag, job)) = jobs.pop_front() {
                if tag.live(now) {
                    return Some((tag, job));
                }
            }
            None
        }
        if let Some((tag, job)) = front(&self.promise_jobs, now) {
            return Some((tag, ReadyJob::Promise(job)));
        }
        if let Some((tag, job)) = front(&self.generic_jobs, now) {
            return Some((tag, ReadyJob::Generic(job)));
        }
        if let Some((tag, job)) = front(&self.async_jobs, now) {
            return Some((tag, ReadyJob::Async(job)));
        }
        let mut timers = self.timers.borrow_mut();
        while timers
            .first_key_value()
            .is_some_and(|((due, _), _)| *due <= now)
        {
            if let Some((_, (tag, job))) = timers.pop_first()
                && tag.live(now)
            {
                return Some((tag, ReadyJob::Timer(job)));
            }
        }
        None
    }

    /// Give every job and timer of `from` to `to`, to be dropped once `expiry` (`from`'s deadline)
    /// passes, or the earlier expiry it already had.
    pub(crate) fn retag(&self, from: RunId, to: RunId, expiry: Instant) {
        let moved = |tag: &mut Tag| {
            if *tag == from {
                *tag = Tag {
                    run: to,
                    expiry: Some(tag.expiry.map_or(expiry, |had| had.min(expiry))),
                };
            }
        };
        self.promise_jobs
            .borrow_mut()
            .iter_mut()
            .for_each(|(r, _)| moved(r));
        self.generic_jobs
            .borrow_mut()
            .iter_mut()
            .for_each(|(r, _)| moved(r));
        self.async_jobs
            .borrow_mut()
            .iter_mut()
            .for_each(|(r, _)| moved(r));
        self.timers
            .borrow_mut()
            .values_mut()
            .for_each(|(r, _)| moved(r));
    }

    /// [`pop_ready`](Self::pop_ready), for `run`'s jobs only.
    fn pop_ready_for(&self, run: RunId, now: Instant) -> Option<ReadyJob> {
        fn take<J>(jobs: &RefCell<VecDeque<(Tag, J)>>, run: RunId) -> Option<J> {
            let mut jobs = jobs.borrow_mut();
            let at = jobs.iter().position(|(r, _)| *r == run)?;
            jobs.remove(at).map(|(_, job)| job)
        }
        if let Some(job) = take(&self.promise_jobs, run) {
            return Some(ReadyJob::Promise(job));
        }
        if let Some(job) = take(&self.generic_jobs, run) {
            return Some(ReadyJob::Generic(job));
        }
        if let Some(job) = take(&self.async_jobs, run) {
            return Some(ReadyJob::Async(job));
        }
        let mut timers = self.timers.borrow_mut();
        let key = timers
            .iter()
            .find(|((due, _), (r, _))| *r == run && *due <= now)
            .map(|(key, _)| *key)?;
        timers.remove(&key).map(|(_, job)| ReadyJob::Timer(job))
    }

    /// Whether a job is ready without waiting for a timer.
    pub(crate) fn has_ready(&self) -> bool {
        !self.promise_jobs.borrow().is_empty()
            || !self.generic_jobs.borrow().is_empty()
            || !self.async_jobs.borrow().is_empty()
    }

    /// When the first timer of any run is due.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.timers.borrow().keys().next().map(|(due, _)| *due)
    }

    /// What `run` has queued that has not expired.
    pub(crate) fn queued_for(&self, run: RunId) -> RunJobs {
        let now = Instant::now();
        let mine = |tag: &Tag| *tag == run && tag.live(now);
        let ready = self
            .promise_jobs
            .borrow()
            .iter()
            .filter(|(t, _)| mine(t))
            .count()
            + self
                .generic_jobs
                .borrow()
                .iter()
                .filter(|(t, _)| mine(t))
                .count()
            + self
                .async_jobs
                .borrow()
                .iter()
                .filter(|(t, _)| mine(t))
                .count();
        let next_timer = self
            .timers
            .borrow()
            .iter()
            .find(|(_, (t, _))| mine(t))
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
            if let Some(job) = self.pop_ready_for(run, now) {
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
        let run = Tag {
            run,
            expiry: self.expiry.get(),
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

    /// Give the timers `from` set to `to`.
    pub(crate) fn retag(&self, from: RunId, to: RunId) {
        for owner in self.table.borrow_mut().1.values_mut() {
            if *owner == from.raw() {
                *owner = to.raw();
            }
        }
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
        while let Some((tag, job)) = jobs.pop_ready(Instant::now()) {
            jobs.set_current(Some(tag.run));
            job.run(context).expect("job");
            ran.push(tag.run);
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

    #[test]
    fn retag_gives_a_runs_jobs_and_timers_to_another() {
        let jobs = Rc::new(MbJobExecutor::new());
        let mut context = crate::scripting::js_engine::sealed_js_context_on(Rc::clone(&jobs));
        let timers = install_timers(&mut context).expect("timers");
        let a = RunId::next();
        jobs.set_current(Some(a));
        eval(
            &mut context,
            "Promise.resolve().then(() => {}); setTimeout(() => {}, 0);",
        );
        let b = RunId::next();
        jobs.retag(a, b, Instant::now() + Duration::from_secs(60));
        timers.retag(a, b);
        timers.forget_run(a);
        assert_eq!(timers.live(), 1, "b owns the timer now");
        assert_eq!(jobs.queued_for(a).ready, 0);
        assert_eq!(jobs.queued_for(b).ready, 1);
        assert!(jobs.queued_for(b).next_timer.is_some());
    }

    // A job a run inherited is dropped at the deadline of the run that queued it, and so are the
    // jobs it queues in turn: an endless chain handed from run to run still ends.
    #[test]
    fn an_inherited_chain_ends_at_its_first_runs_deadline() {
        let (mut context, jobs, a) = context();
        eval(
            &mut context,
            "var n = 0; (async function f() { n++; await 0; f(); })();",
        );
        let b = RunId::next();
        let expiry = Instant::now() + Duration::from_millis(50);
        jobs.retag(a, b, expiry);
        let mut ran = 0;
        while let Some((tag, job)) = jobs.pop_ready(Instant::now()) {
            assert_eq!((tag.run, tag.expiry), (b, Some(expiry)));
            jobs.set_current(Some(tag.run));
            let previous = jobs.set_expiry(tag.expiry);
            job.run(&mut context).expect("job");
            jobs.set_expiry(previous);
            ran += 1;
            assert!(ran < 10_000_000, "the chain never ended");
        }
        assert!(
            Instant::now() >= expiry,
            "the chain ended before its expiry"
        );
        assert_eq!(
            jobs.queued_for(b),
            RunJobs {
                ready: 0,
                next_timer: None
            }
        );
    }
}
