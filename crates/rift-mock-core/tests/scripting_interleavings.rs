//! Runs of the script state cell under a probe that can hold the engine (`formal/scripting`).
//!
//! The MB script pool is process-global, so the probe rides the run's `ScriptScope`, which
//! `ImposterManager::with_probe` gives every imposter it creates. A held point blocks a pool worker,
//! which is what makes these interleavings forced rather than hoped for; the latch is bounded, so a
//! broken test fails rather than wedges the pool for the other tests in this binary.
//!
//! A test marked `#[ignore = "#n"]` states the correct behaviour and fails today: it reproduces the
//! defect filed as issue n. `cargo test --test scripting_interleavings -- --ignored` runs them, and
//! the fix removes the attribute.
#![cfg(feature = "javascript")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar};
use std::time::Duration;

use parking_lot::Mutex;
use rift_mock_core::imposter::{ImposterConfig, ImposterManager, Point, Probe};
use serde_json::{Value, json};

/// A door the engine waits at until the test opens it.
#[derive(Default)]
struct Latch {
    open: std::sync::Mutex<bool>,
    changed: Condvar,
}

impl Latch {
    fn open(&self) {
        *self.open.lock().expect("latch") = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let guard = self.open.lock().expect("latch");
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(20), |open| !*open)
            .expect("latch");
        assert!(
            !timeout.timed_out(),
            "a held probe point was never released"
        );
    }
}

type Matcher = fn(&Point) -> bool;

/// A point the engine took, and the thread it took it on.
#[derive(Clone, Debug)]
struct Seen {
    seq: u64,
    point: Point,
    thread: String,
}

/// Records the script points of one imposter, and holds the engine at the ones a test asks for.
/// Pool threads are not tokio's, so a hold is a plain block.
#[derive(Default)]
struct Recorder {
    seq: AtomicU64,
    points: Mutex<Vec<Seen>>,
    holds: Mutex<Vec<(Matcher, Arc<Latch>)>>,
}

impl Probe for Recorder {
    fn at(&self, point: Point) {
        if !is_script_point(&point) {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        self.points.lock().push(Seen { seq, point, thread });
        let held = {
            let mut holds = self.holds.lock();
            holds
                .iter()
                .position(|(matches, _)| matches(&point))
                .map(|i| holds.remove(i).1)
        };
        if let Some(latch) = held {
            latch.wait();
        }
    }
}

impl Recorder {
    fn hold(&self, matches: Matcher) -> Arc<Latch> {
        let latch = Arc::new(Latch::default());
        self.holds.lock().push((matches, Arc::clone(&latch)));
        latch
    }

    fn seen(&self) -> Vec<Seen> {
        let mut seen = self.points.lock().clone();
        seen.sort_by_key(|s| s.seq);
        seen
    }

    async fn reached(&self, matches: Matcher) -> Seen {
        for _ in 0..2000 {
            if let Some(seen) = self.seen().into_iter().find(|s| matches(&s.point)) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the engine never reached the point: {:?}", self.seen());
    }
}

fn is_script_point(point: &Point) -> bool {
    matches!(
        point,
        Point::ScriptSeeded { .. }
            | Point::ScriptSaved { .. }
            | Point::ScriptFinished { .. }
            | Point::ScriptReleased { .. }
    )
}

fn seeded(p: &Point) -> bool {
    matches!(p, Point::ScriptSeeded { .. })
}

fn saved(p: &Point) -> bool {
    matches!(p, Point::ScriptSaved { .. })
}

fn key_of(point: &Point) -> usize {
    match point {
        Point::ScriptSeeded { key, .. }
        | Point::ScriptSaved { key, .. }
        | Point::ScriptFinished { key, .. }
        | Point::ScriptReleased { key, .. } => *key,
        other => panic!("not a script point: {other:?}"),
    }
}

fn manager(recorder: &Arc<Recorder>) -> ImposterManager {
    ImposterManager::new().with_probe(Arc::clone(recorder) as Arc<dyn Probe>)
}

fn imposter(stubs: Value) -> ImposterConfig {
    serde_json::from_value(json!({
        "port": 0,
        "host": "127.0.0.1",
        "protocol": "http",
        "stubs": stubs
    }))
    .expect("imposter config")
}

fn inject_on(path: &str, script: &str) -> Value {
    json!({
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{ "inject": script }]
    })
}

async fn get(port: u16, path: &str) -> (u16, String) {
    let response = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("a response");
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

/// Spawn a request, so the test can act while the engine is held on it.
fn spawn_get(port: u16, path: &'static str) -> tokio::task::JoinHandle<(u16, String)> {
    tokio::spawn(async move { get(port, path).await })
}

// ---- What the model's End says, on the engine (formal/scripting, EndSave -> EndRelease). ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_saves_then_answers_then_lets_go_of_its_claim() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([inject_on(
            "/",
            "config => { config.state.n = 1; return { body: 'x' }; }"
        )])))
        .await
        .expect("create");

    let (status, body) = get(port, "/").await;
    assert_eq!((status, body.as_str()), (200, "x"));

    let order: Vec<&'static str> = recorder
        .seen()
        .iter()
        .map(|s| match s.point {
            Point::ScriptSeeded { .. } => "seeded",
            Point::ScriptSaved { .. } => "saved",
            Point::ScriptFinished { .. } => "finished",
            Point::ScriptReleased { .. } => "released",
            _ => "other",
        })
        .collect();
    assert_eq!(order, ["seeded", "saved", "finished", "released"]);
    let _ = manager.delete_all().await;
}

/// `CellCurrentBeforeAnswer` with the answer forced to wait: the cell the next run seeds from holds
/// what the first wrote, and the next run reaches the worker holding the imposter only then.
/// `NeverTwoWorkersOneKey`: held at its save, the first run pins the imposter's next run to its
/// worker, although the pool has idle workers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_held_at_its_save_keeps_the_imposters_next_run_behind_it() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([inject_on(
            "/",
            "config => { config.state.n = (config.state.n || 0) + 1; \
             return { body: String(config.state.n) }; }"
        )])))
        .await
        .expect("create");

    let latch = recorder.hold(saved);
    let first = spawn_get(port, "/");
    recorder.reached(saved).await;
    let second = spawn_get(port, "/");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        recorder.seen().iter().filter(|s| saved(&s.point)).count(),
        1,
        "the next run ran beside the held one: {:?}",
        recorder.seen()
    );
    assert!(!first.is_finished(), "answered before the save completed");
    latch.open();

    assert_eq!(first.await.expect("first").1, "1");
    assert_eq!(second.await.expect("second").1, "2");
    let threads: std::collections::BTreeSet<String> =
        recorder.seen().into_iter().map(|s| s.thread).collect();
    assert_eq!(threads.len(), 1, "two workers ran one key: {threads:?}");
    let _ = manager.delete_all().await;
}

/// The witness `NeverSaveToOldCell` (#1726): a run in flight across a restart saves to the cell it
/// started on, and the imposter's next runs start from the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_in_flight_across_a_restart_saves_to_its_old_cell() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            inject_on(
                "/slow",
                "async config => { config.state.n = 1; \
                 await new Promise(r => setTimeout(r, 300)); \
                 config.state.n += 1; return { body: String(config.state.n) }; }"
            ),
            inject_on("/read", "config => ({ body: String(config.state.n) })")
        ])))
        .await
        .expect("create");

    let slow = spawn_get(port, "/slow");
    let old = recorder.reached(seeded).await;
    manager.restart_runtime_state(port).expect("restart");

    assert_eq!(get(port, "/read").await.1, "undefined");
    assert_eq!(slow.await.expect("slow").1, "2");
    let seen = recorder.seen();
    let old_key = key_of(&old.point);
    let last_save = seen
        .iter()
        .rfind(|s| saved(&s.point))
        .expect("a save")
        .clone();
    assert_eq!(key_of(&last_save.point), old_key);
    assert!(
        seen.iter()
            .any(|s| saved(&s.point) && key_of(&s.point) != old_key),
        "the run after the restart had a cell of its own: {seen:?}"
    );
    assert_eq!(
        get(port, "/read").await.1,
        "undefined",
        "the old run's save reached the new cell"
    );
    let _ = manager.delete_all().await;
}

/// The imposter's realm changes while a run is in flight: the next run waits on `thread.deferred`,
/// rather than take a second live state in the other realm, and sees what the first one saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_in_another_realm_waits_for_the_imposters_run_in_flight() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            inject_on(
                "/slow",
                "async config => { config.state.n = 1; \
                 await new Promise(r => setTimeout(r, 300)); \
                 config.state.n += 1; return { body: String(config.state.n) }; }"
            ),
            inject_on("/read", "config => ({ body: String(config.state.n) })")
        ])))
        .await
        .expect("create");

    let slow = spawn_get(port, "/slow");
    recorder.reached(seeded).await;
    manager
        .set_script_file_access(
            port,
            rift_mock_core::imposter::ScriptFileAccess::Confined(std::env::temp_dir()),
        )
        .expect("set access");

    assert_eq!(get(port, "/read").await.1, "2");
    assert_eq!(slow.await.expect("slow").1, "2");
    assert_eq!(
        recorder.seen().iter().filter(|s| seeded(&s.point)).count(),
        2,
        "the deferred run seeds anew from the cell, in its own realm"
    );
    let _ = manager.delete_all().await;
}

// ---- Findings: each fails today (formal/scripting/README.md, "Findings"). ----

/// One request's predicate inject and response inject take a scope each, so a restart between them
/// splits the request across two cells, and the inject does not see what the predicate wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#1784"]
async fn a_request_keeps_one_state_across_a_restart() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([{
            "predicates": [{ "inject": "config => { config.state.mark = 'seen'; return true; }" }],
            "responses": [{ "inject": "config => ({ body: String(config.state.mark) })" }]
        }])))
        .await
        .expect("create");

    // Held in the predicate run's end, after its save: the request is past its scope for the match
    // and has not taken the one for the inject.
    let latch = recorder.hold(saved);
    let request = spawn_get(port, "/");
    recorder.reached(saved).await;
    manager.restart_runtime_state(port).expect("restart");
    latch.open();

    let (status, body) = request.await.expect("request");
    let _ = manager.delete_all().await;
    assert_eq!(status, 200);
    assert_eq!(
        body, "seen",
        "the inject ran on another cell than the predicate"
    );
}

/// Reading the answer out of its JS object runs any getter on it. That is script, so it comes before
/// the save, or its write goes to a live state nobody reads again (issue #1785).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_made_while_the_answer_is_read_is_not_lost() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([inject_on(
            "/",
            "config => { const seen = config.state.n; \
             return { get body() { config.state.n = 1; return String(seen); } }; }"
        )])))
        .await
        .expect("create");

    let first = get(port, "/").await;
    let second = get(port, "/").await;
    let _ = manager.delete_all().await;
    assert_eq!(first.1, "undefined");
    assert_eq!(
        second.1, "1",
        "the getter's write did not reach the next run"
    );
}

/// As above for a thrown object: reading its `message` runs a getter too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_made_while_a_thrown_value_is_read_is_not_lost() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            inject_on(
                "/throw",
                "config => { throw { get message() { config.state.n = 1; return 'boom'; } }; }"
            ),
            inject_on("/read", "config => ({ body: String(config.state.n) })")
        ])))
        .await
        .expect("create");

    let (status, _) = get(port, "/throw").await;
    let (_, read) = get(port, "/read").await;
    let _ = manager.delete_all().await;
    assert_eq!(status, 500, "it still fails as a throw");
    assert_eq!(read, "1", "the getter's write did not reach the next run");
}

/// As above for a predicate that throws.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_made_while_a_predicates_thrown_value_is_read_is_not_lost() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            {
                "predicates": [
                    { "equals": { "path": "/predicate" } },
                    { "inject": "config => { throw { get message() { \
                        config.state.n = 1; return 'boom'; } }; }" }
                ],
                "responses": [{ "is": { "body": "matched" } }]
            },
            inject_on("/read", "config => ({ body: String(config.state.n) })")
        ])))
        .await
        .expect("create");

    let _ = get(port, "/predicate").await;
    let (_, read) = get(port, "/read").await;
    let _ = manager.delete_all().await;
    assert_eq!(read, "1", "the getter's write did not reach the next run");
}

/// As above for a decorate, whose response is read after it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_made_while_a_decorated_response_is_read_is_not_lost() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            {
                "predicates": [{ "equals": { "path": "/decorate" } }],
                "responses": [{
                    "is": { "statusCode": 200, "body": "plain" },
                    "behaviors": [{ "decorate":
                        "config => { const response = config.response; \
                         Object.defineProperty(config, 'response', { get() { \
                         config.state.n = 1; return response; } }); }"
                    }]
                }]
            },
            inject_on("/read", "config => ({ body: String(config.state.n) })")
        ])))
        .await
        .expect("create");

    let (_, body) = get(port, "/decorate").await;
    let (_, read) = get(port, "/read").await;
    let _ = manager.delete_all().await;
    assert_eq!(body, "plain");
    assert_eq!(read, "1", "the getter's write did not reach the next run");
}

/// What a finished run left passes to a run of its imposter in flight (`MbRealm::end`), and a job
/// that throws ends the run it is tagged with (`MbRealm::step`): so a timer left by one request
/// fails another request that has nothing to do with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_left_behind_does_not_fail_the_run_that_inherited_it() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager
        .create_imposter(imposter(json!([
            inject_on(
                "/waits",
                "config => new Promise(r => setTimeout(() => r({ body: 'waited' }), 400))"
            ),
            inject_on(
                "/leaves",
                "config => { setTimeout(() => { throw new Error('left behind'); }, 50); \
                 return { body: 'left' }; }"
            )
        ])))
        .await
        .expect("create");

    let waits = spawn_get(port, "/waits");
    recorder.reached(seeded).await;
    let (_, left) = get(port, "/leaves").await;
    assert_eq!(left, "left");
    let (status, body) = waits.await.expect("waits");
    let _ = manager.delete_all().await;
    assert_eq!(
        (status, body.as_str()),
        (200, "waited"),
        "another request's left-behind timer failed this one"
    );
}
