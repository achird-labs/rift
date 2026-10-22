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
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

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

/// The job executor of an MB pool realm (issue #1593). FIFO promise, generic and async jobs, and
/// timers ordered by due time; see the module doc.
///
/// An async job (a dynamic `import()`, which the sealed module loader refuses) is a future, polled
/// within the drain that started it; one still pending when the drain ends is dropped with it.
/// `Atomics.waitAsync`, whose waiter would outlive that drop in Boa's process-wide futex list, is
/// removed from every script context (`js_engine::build_sealed_js_context`). `FinalizationRegistry` cleanups are dropped when
/// queued: the spec lets a host skip them, and they would otherwise wait for a collection that
/// may never come.
#[derive(Default)]
pub(crate) struct MbJobExecutor {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    /// Keyed by due time, then by enqueue order, so timers due together run in the order set.
    timers: RefCell<BTreeMap<(Instant, u64), TimeoutJob>>,
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

    /// Drop every queued job without running it.
    pub(crate) fn clear(&self) {
        self.promise_jobs.borrow_mut().clear();
        self.generic_jobs.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
        self.timers.borrow_mut().clear();
    }

    /// Run queued jobs until `finished` holds, no job is left, or `deadline` passes. Promise and
    /// generic jobs run first, as Node runs microtasks before the next timer; when only timers
    /// are left, the thread sleeps until the first is due. A timer due after `deadline` is not
    /// waited for: the run ends with [`Drained::Deadline`] at once. A job that throws ends the
    /// drain with its error, and drops what is left.
    pub(crate) fn drain(
        &self,
        context: &mut Context,
        deadline: Instant,
        mut finished: impl FnMut(&mut Context) -> bool,
    ) -> JsResult<Drained> {
        let outcome = self.drain_inner(context, deadline, &mut finished);
        if outcome.is_err() {
            self.clear();
        }
        context.clear_kept_objects();
        outcome
    }

    fn drain_inner(
        &self,
        context: &mut Context,
        deadline: Instant,
        finished: &mut impl FnMut(&mut Context) -> bool,
    ) -> JsResult<Drained> {
        type AsyncRun<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = JsResult<JsValue>> + 'a>>;
        // Async jobs take the context through a `RefCell`, as Boa's own executor runs them; every
        // other job borrows it for the length of its call.
        let context = RefCell::new(context);
        let mut running: Vec<AsyncRun<'_>> = Vec::new();
        let mut poll = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if finished(&mut context.borrow_mut()) {
                return Ok(Drained::Finished);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Drained::Deadline);
            }
            // Each pop releases its borrow before the job runs: a job enqueues more.
            let promise_job = self.promise_jobs.borrow_mut().pop_front();
            if let Some(job) = promise_job {
                job.call(&mut context.borrow_mut())?;
                continue;
            }
            let generic_job = self.generic_jobs.borrow_mut().pop_front();
            if let Some(job) = generic_job {
                job.call(&mut context.borrow_mut())?;
                continue;
            }
            let started = std::mem::take(&mut *self.async_jobs.borrow_mut());
            running.extend(
                started
                    .into_iter()
                    .map(|job| Box::pin(job.call(&context)) as AsyncRun<'_>),
            );
            let before = running.len();
            let mut failed = None;
            running.retain_mut(|run| match run.as_mut().poll(&mut poll) {
                std::task::Poll::Ready(Ok(_)) => false,
                std::task::Poll::Ready(Err(e)) => {
                    failed.get_or_insert(e);
                    false
                }
                std::task::Poll::Pending => true,
            });
            if let Some(e) = failed {
                return Err(e);
            }
            if running.len() < before {
                // A finished async job settles a promise: its reactions run first.
                continue;
            }
            let next_due = self.timers.borrow().keys().next().map(|(due, _)| *due);
            let Some(due) = next_due else {
                if running.is_empty() {
                    return Ok(Drained::Idle);
                }
                // Only pending async jobs are left, and a no-op waker says nothing of when one
                // can go on: look again shortly, until the deadline.
                std::thread::sleep(Duration::from_millis(1).min(deadline - now));
                continue;
            };
            if due > deadline {
                return Ok(Drained::Deadline);
            }
            if due > now {
                let wait = due - now;
                // With async jobs pending, wake to poll them meanwhile.
                std::thread::sleep(if running.is_empty() {
                    wait
                } else {
                    wait.min(Duration::from_millis(1))
                });
                if Instant::now() < due {
                    continue;
                }
            }
            let timer = self.timers.borrow_mut().pop_first();
            if let Some((_, job)) = timer {
                job.call(&mut context.borrow_mut())?;
            }
        }
    }
}

impl JobExecutor for MbJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            Job::PromiseJob(job) => self.promise_jobs.borrow_mut().push_back(job),
            Job::GenericJob(job) => self.generic_jobs.borrow_mut().push_back(job),
            Job::AsyncJob(job) => self.async_jobs.borrow_mut().push_back(job),
            Job::TimeoutJob(job) => {
                let due = Instant::now() + Duration::from_millis(job.timeout().as_millis());
                let seq = self.next_timer.get();
                self.next_timer.set(seq.wrapping_add(1));
                self.timers.borrow_mut().insert((due, seq), job);
            }
            // Dropped by design: a `FinalizationRegistry` cleanup (see the type's doc), or an
            // interval job, which only `boa_runtime`'s timers queue (Rift's re-queue timeouts).
            other => tracing::debug!(
                target: "rift::script",
                "a script queued a job the MB script runtime does not run: {other:?}"
            ),
        }
    }

    /// Runs the queued jobs to the end, within [`crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS`].
    /// The hooks never call this (they call [`MbJobExecutor::drain`] with their own deadline);
    /// it exists for Boa's API and keeps its bound.
    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        let deadline =
            Instant::now() + Duration::from_millis(crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS);
        match self.drain(context, deadline, |_| false)? {
            Drained::Deadline => Err(JsNativeError::error()
                .with_message("script jobs did not finish within the script timeout")
                .into()),
            Drained::Finished | Drained::Idle => Ok(()),
        }
    }
}

/// The ids of one run's live timers, and the next id to hand out. A cleared or fired timeout is
/// not live; an interval is live until cleared. GC-traced, so it is a plain capture of the
/// timer natives (no `unsafe`).
type TimerTable = boa_engine::gc::Gc<boa_engine::gc::GcRefCell<(u32, HashSet<u32>)>>;

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
            if !timers.borrow().1.contains(&id) {
                return Ok(JsValue::undefined());
            }
            if !repeat {
                timers.borrow_mut().1.remove(&id);
            }
            callback.call(&JsValue::undefined(), &args, context)?;
            if repeat && timers.borrow().1.contains(&id) {
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
        table.1.insert(id);
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

/// Set `setTimeout`, `clearTimeout`, `setInterval` and `clearInterval` on the global object, over
/// a fresh timer table (issue #1593). A timer's callback runs only inside a drain of this context
/// (see [`MbJobExecutor`]); a context with another executor queues it and never runs it.
pub(crate) fn install_timers(context: &mut Context) -> JsResult<()> {
    let timers: TimerTable =
        boa_engine::gc::Gc::new(boa_engine::gc::GcRefCell::new((0, HashSet::new())));
    let natives: [(&str, TimerFn); 4] = [
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
    let global: JsObject = context.global_object();
    for (name, func) in natives {
        let native = NativeFunction::from_copy_closure_with_captures(func, timers.clone())
            .to_js_function(context.realm());
        global.set(js_string!(name), native, false, context)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> (Context, Rc<MbJobExecutor>) {
        let jobs = Rc::new(MbJobExecutor::new());
        let mut context = crate::scripting::js_engine::sealed_js_context_on(Rc::clone(&jobs));
        install_timers(&mut context).expect("timers");
        (context, jobs)
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
        let (mut context, jobs) = context();
        eval(
            &mut context,
            "var order = []; setTimeout(() => order.push('t'), 0); \
             Promise.resolve().then(() => order.push('p'));",
        );
        let end = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        let order = global(&mut context, "order");
        let order = order.to_string(&mut context).expect("string");
        assert_eq!(order.to_std_string_escaped(), "p,t");
    }

    #[test]
    fn an_interval_runs_until_cleared() {
        let (mut context, jobs) = context();
        eval(
            &mut context,
            "var n = 0; var id = setInterval(() => { n++; if (n === 3) clearInterval(id); }, 1);",
        );
        let end = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        assert_eq!(global(&mut context, "n"), JsValue::from(3));
    }

    #[test]
    fn an_endless_microtask_chain_ends_at_the_deadline() {
        let (mut context, jobs) = context();
        eval(&mut context, "async function f() { await 0; f(); } f();");
        let started = Instant::now();
        let end = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_millis(50),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Deadline);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_timer_due_after_the_deadline_is_not_waited_for() {
        let (mut context, jobs) = context();
        eval(&mut context, "setTimeout(() => {}, 10000);");
        let started = Instant::now();
        let end = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_millis(100),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Deadline);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn a_throwing_timer_ends_the_drain_and_drops_the_rest() {
        let (mut context, jobs) = context();
        eval(
            &mut context,
            "var later = false; setTimeout(() => { throw new Error('tick'); }, 0); \
             setTimeout(() => { later = true; }, 1);",
        );
        let err = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect_err("the throw");
        assert!(err.to_string().contains("tick"), "{err}");
        let end = jobs
            .drain(
                &mut context,
                Instant::now() + Duration::from_secs(1),
                |_| false,
            )
            .expect("drain");
        assert_eq!(end, Drained::Idle);
        assert_eq!(global(&mut context, "later"), JsValue::from(false));
    }

    #[test]
    fn set_timeout_needs_a_function() {
        let (mut context, _jobs) = context();
        let err = context
            .eval(boa_engine::Source::from_bytes(b"setTimeout('1 + 1', 1)"))
            .expect_err("a string is no callback");
        assert!(err.to_string().contains("callback"), "{err}");
    }
}
