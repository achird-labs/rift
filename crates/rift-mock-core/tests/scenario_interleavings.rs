//! Scenario transitions under a holding probe (`formal/scenario`).
//!
//! A probe holds the real engine at a point the model names, which forces an interleaving
//! `Scenario.tla` found instead of hoping the scheduler lands in a window a few instructions wide.
//! Blocking a worker this way is for tests only, so these run on a multi-threaded runtime with
//! workers to spare.
//!
//! A test marked `#[ignore = "#n"]` states the correct behaviour and fails today: it reproduces the
//! defect filed as issue n. `cargo test --test scenario_interleavings -- --ignored` runs them, and
//! the fix removes the attribute.
//!
//! Every test runs on the model's own stub table ([`model_table`]). With `FORMAL_TRACE_DIR` set, each
//! writes its run there as ndjson, for `formal/scenario/validate.py` to check against the model:
//!
//! ```text
//! FORMAL_TRACE_DIR=$PWD/formal/scenario/traces/accept \
//!   cargo test -p rift-mock-core --test scenario_interleavings -- --include-ignored
//! ```

use std::sync::{Arc, Condvar};
use std::time::Duration;

use parking_lot::Mutex;
use rift_mock_core::imposter::{ImposterManager, Point, Probe};
use serde_json::{Value, json};

/// A door the engine waits at until the test opens it. Bounded, so a broken test fails rather than
/// hangs.
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
            .wait_timeout_while(guard, Duration::from_secs(30), |open| !*open)
            .expect("latch");
        assert!(
            !timeout.timed_out(),
            "a held probe point was never released"
        );
    }
}

type Matcher = fn(&Point) -> bool;

/// Records every point, and holds the engine at the points a test asks for.
#[derive(Default)]
struct Recorder {
    points: Mutex<Vec<Point>>,
    holds: Mutex<Vec<(Matcher, Arc<Latch>)>>,
}

impl Probe for Recorder {
    fn at(&self, point: Point) {
        self.points.lock().push(point);
        let held = {
            let mut holds = self.holds.lock();
            holds
                .iter()
                .position(|(matches, _)| matches(&point))
                .map(|i| holds.remove(i).1)
        };
        if let Some(latch) = held {
            // Not a plain block: handing the worker over lets a task in its LIFO slot run.
            tokio::task::block_in_place(|| latch.wait());
        }
    }
}

impl Recorder {
    /// Hold the engine the next time it reaches a matching point, until the latch opens.
    fn hold(&self, matches: Matcher) -> Arc<Latch> {
        let latch = Arc::new(Latch::default());
        self.holds.lock().push((matches, Arc::clone(&latch)));
        latch
    }

    /// Wait until the engine has reached a matching point.
    async fn reached(&self, matches: Matcher) {
        for _ in 0..2000 {
            if self.points.lock().iter().any(matches) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the engine never reached the awaited point: {:?}",
            self.points.lock()
        );
    }

    /// Write the run to `$FORMAL_TRACE_DIR/<name>.ndjson`, when the variable is set. `start` is the
    /// run's configuration as `Scenario.tla` names it (see [`roles`]).
    fn write(&self, name: &str, mut start: Value) {
        let Ok(dir) = std::env::var("FORMAL_TRACE_DIR") else {
            return;
        };
        start["point"] = json!("Start");
        let mut lines = start.to_string();
        lines.push('\n');
        let points = self.points.lock();
        // A stamp is written as the model's counter: the k-th stamp issued is k (the load clock
        // never repeats or goes back), and one older than every transition, a stub's own load
        // stamp, is 0. Anything else no step serves, so the trace is rejected.
        let mut issued: Vec<i64> = points
            .iter()
            .filter_map(|p| match p {
                Point::TransitionStampIssued { stamp, .. } => Some(*stamp),
                _ => None,
            })
            .collect();
        issued.sort_unstable();
        let counter = |stamp: i64| match issued.iter().position(|s| *s == stamp) {
            Some(k) => json!(k + 1),
            None if issued.first().is_none_or(|first| stamp < *first) => json!(0),
            None => json!(-1),
        };
        // The manager's other points (create, delete) are no part of this model.
        for mut point in points.iter().map(to_json) {
            if point["point"] == "Other" {
                continue;
            }
            if let Some(stamp) = point["stamp"].as_i64() {
                point["stamp"] = counter(stamp);
            }
            lines.push_str(&point.to_string());
            lines.push('\n');
        }
        std::fs::create_dir_all(&dir).expect("trace dir");
        std::fs::write(format!("{dir}/{name}.ndjson"), lines).expect("write trace");
    }
}

/// A point as `formal/scenario/TraceValidation.tla` labels it. A gate read names the model's request
/// role and the gate's position in `Order`, from where [`model_table`] puts the stub.
fn to_json(point: &Point) -> Value {
    match *point {
        Point::GateRead { stub_index, .. } => {
            let (role, pos) = match stub_index {
                0..3 => ("gated", stub_index + 1),
                3..6 => ("poller", stub_index - 2),
                // No stub of the model's table: no step takes it, so the trace is rejected.
                _ => ("unmodelled", 0),
            };
            json!({ "point": "GateRead", "role": role, "pos": pos })
        }
        Point::CasApplied { .. } => json!({ "point": "CasApplied" }),
        Point::CasConflict { .. } => json!({ "point": "CasConflict" }),
        Point::ScenarioChangeRead { changed, .. } => {
            json!({ "point": "ScenarioChangeRead", "changed": changed })
        }
        Point::ScenarioWriteNext { .. } => json!({ "point": "ScenarioWriteNext" }),
        Point::TransitionStampIssued { stamp, .. } => {
            json!({ "point": "TransitionStampIssued", "stamp": stamp })
        }
        Point::TransitionStamped { stamp, .. } => {
            json!({ "point": "TransitionStamped", "stamp": stamp })
        }
        Point::Served { stamp, .. } => json!({ "point": "Served", "stamp": stamp }),
        _ => json!({ "point": "Other" }),
    }
}

/// A run's configuration for its trace: how many requests of each of the model's roles it made.
/// The store starts empty, and a setter writes `B`, as in [`model_table`].
fn roles(gated: usize, pollers: usize, setters: usize, resetters: usize, clearers: usize) -> Value {
    json!({ "gated": gated, "pollers": pollers, "setters": setters, "resetters": resetters,
            "clearers": clearers })
}

fn manager(recorder: &Arc<Recorder>) -> ImposterManager {
    ImposterManager::new().with_probe(Arc::clone(recorder) as Arc<dyn Probe>)
}

/// A conditional stub on `path`, gated on `required` when given, moving to `next` when given.
fn stub(path: &str, required: Option<&str>, next: Option<&str>, body: &str) -> Value {
    let mut stub = json!({
        "scenarioName": "S",
        "predicates": [{ "equals": { "path": path } }],
        "responses": [{
            "is": { "statusCode": 200, "body": body },
            "_rift": { "conditional": true }
        }]
    });
    if let Some(required) = required {
        stub["requiredScenarioState"] = json!(required);
    }
    if let Some(next) = next {
        stub["newScenarioState"] = json!(next);
    }
    stub
}

/// `Scenario.tla`'s stub table. `/go` carries the cycle `Started -> A -> B -> Started` in scan order
/// (the model's `Gated`); `/read` has a read-only stub under each of the same gates, in the same
/// order (`Pollers`); `/set` is ungated and moves the scenario to `B` (`Setters`, `SetTo`). Each
/// conditional stub answers the state its gate saw.
fn model_table() -> Value {
    json!([
        stub("/go", Some("Started"), Some("A"), "started"),
        stub("/go", Some("A"), Some("B"), "a"),
        stub("/go", Some("B"), Some("Started"), "b"),
        stub("/read", Some("Started"), None, "started"),
        stub("/read", Some("A"), None, "a"),
        stub("/read", Some("B"), None, "b"),
        {
            "scenarioName": "S",
            "newScenarioState": "B",
            "predicates": [{ "equals": { "path": "/set" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "set" } }]
        },
    ])
}

async fn create(manager: &ImposterManager) -> u16 {
    let config = serde_json::from_value(json!({
        "port": 0,
        "host": "127.0.0.1",
        "protocol": "http",
        "stubs": model_table()
    }))
    .expect("imposter config");
    manager.create_imposter(config).await.expect("create")
}

#[derive(Debug)]
struct Answer {
    status: u16,
    body: String,
    last_modified: Option<String>,
    no_match: bool,
}

/// A request on a fresh connection.
async fn get(port: u16, path: &str, since: Option<&str>) -> Answer {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(40))
        .build()
        .expect("client");
    let mut request = client.get(format!("http://127.0.0.1:{port}{path}"));
    if let Some(since) = since {
        request = request.header("if-modified-since", since);
    }
    let response = request.send().await.expect("send");
    let status = response.status().as_u16();
    let last_modified = response
        .headers()
        .get("last-modified")
        .map(|v| v.to_str().expect("header").to_owned());
    let no_match = response.headers().contains_key("x-rift-no-match");
    let body = response.text().await.expect("body");
    Answer {
        status,
        body,
        last_modified,
        no_match,
    }
}

/// `Scenario.tla`, `StampMonotone`: the regression's three transitions. The first issues its stamp
/// and stalls before publishing it; the other two run to the end, the last serving its answer under
/// the second stamp; then the first publishes, and must not take the map back to the lowest stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_stamp_does_not_take_the_scenario_back() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;

    let latch = recorder.hold(|p| matches!(p, Point::TransitionStampIssued { .. }));
    let stalled = tokio::spawn(async move { get(port, "/go", None).await });
    recorder
        .reached(|p| matches!(p, Point::TransitionStampIssued { .. }))
        .await;

    // Moves `A -> B`, issues the second stamp and publishes it; answers `a` under no stamp yet.
    assert_eq!(get(port, "/go", None).await.body, "a");
    // Copies the second stamp, moves `B -> Started` under the third, and answers `b` under the
    // second. This client now holds `b`.
    let held = get(port, "/go", None).await;
    assert_eq!(held.body, "b");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    assert_eq!(stalled.await.expect("request").body, "started");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "started"),
        "the client holding `b` under {since} is told about `Started`, Last-Modified {:?}",
        now.last_modified
    );
    recorder.write("stalled_stamp", roles(3, 1, 0, 0, 0));
    manager.delete_all().await;
}

/// `Scenario.tla`, `NoStaleAnswerAtRest`: a setter decides "no change" while the state is what it
/// writes, a gated transition moves the state and stamps, and the setter's write moves it back,
/// which must be stamped too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_that_decided_no_change_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    assert_eq!(get(port, "/set", None).await.body, "set");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }));
    let setter = tokio::spawn(async move { get(port, "/set", None).await });
    recorder
        .reached(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "b");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "started");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    assert_eq!(setter.await.expect("request").body, "set");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "b"),
        "the client holding `started` under {since} is told the state went back to `B`"
    );
    recorder.write("set_no_change", roles(1, 2, 2, 0, 0));
    manager.delete_all().await;
}

/// The same window through the admin door: `flow_delete` is about to delete an absent key, a gated
/// transition moves the state and stamps, and the delete returns it to `Started`, which it stamps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_that_decided_no_change_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    let imposter = manager.get_imposter(port).expect("imposter");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioWriteNext { .. }));
    let flow = port.to_string();
    let delete = tokio::spawn(async move { imposter.flow_delete(&flow, "S") });
    recorder
        .reached(|p| matches!(p, Point::ScenarioWriteNext { .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "started");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "a");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    delete.await.expect("task").expect("delete");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "started"),
        "the client holding `a` under {since} is told the scenario was reset"
    );
    recorder.write("flow_delete", roles(1, 2, 0, 1, 0));
    manager.delete_all().await;
}

/// The same window through `delete_scenario_state`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scenario_reset_about_to_write_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    let imposter = manager.get_imposter(port).expect("imposter");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioWriteNext { .. }));
    let flow = port.to_string();
    let reset = tokio::spawn(async move { imposter.delete_scenario_state(&flow, "S") });
    recorder
        .reached(|p| matches!(p, Point::ScenarioWriteNext { .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "started");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "a");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    reset.await.expect("task").expect("reset");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "started"),
        "the client holding `a` under {since} is told the scenario was reset"
    );
    recorder.write("scenario_reset", roles(1, 2, 0, 1, 0));
    manager.delete_all().await;
}

/// The same window through `flow_clear`, which reports nothing about what it removed, so it finds
/// the transition in the stamp map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_about_to_write_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    let imposter = manager.get_imposter(port).expect("imposter");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioWriteNext { .. }));
    let flow = port.to_string();
    let clear = tokio::spawn(async move { imposter.flow_clear(&flow) });
    recorder
        .reached(|p| matches!(p, Point::ScenarioWriteNext { .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "started");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "a");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    clear.await.expect("task").expect("clear");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "started"),
        "the client holding `a` under {since} is told the flow was cleared"
    );
    recorder.write("flow_clear", roles(1, 2, 0, 0, 1));
    manager.delete_all().await;
}

/// The same window through `flow_set`: it decides "no change" while the state is what it writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flow_set_that_decided_no_change_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    assert_eq!(get(port, "/set", None).await.body, "set");
    let imposter = manager.get_imposter(port).expect("imposter");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }));
    let flow = port.to_string();
    let set = tokio::spawn(async move { imposter.flow_set(&flow, "S", json!("B")) });
    recorder
        .reached(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "b");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "started");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    set.await.expect("task").expect("set");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "b"),
        "the client holding `started` under {since} is told the state went back to `B`"
    );
    recorder.write("flow_set_no_change", roles(1, 2, 2, 0, 0));
    manager.delete_all().await;
}

/// `Scenario.tla`, `NoMatchOnlyWhenUncovered`: a scan reads the state once per scenario. With the
/// state at `B`, the scan passes the gate of `Started`, then of `A`; a transition takes the state to
/// `Started`; the gate of `B` is still judged against the `B` the scan read. Every state has a stub,
/// and the request must match one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scan_of_covered_states_finds_a_stub_while_the_state_moves() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;
    assert_eq!(get(port, "/set", None).await.body, "set");

    // The gate of `/read`'s second stub (`A`) has been judged; the third's has not.
    let at_second_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 4, .. });
    let latch = recorder.hold(at_second_gate);
    let scan = tokio::spawn(async move { get(port, "/read", None).await });
    recorder.reached(at_second_gate).await;

    assert_eq!(get(port, "/go", None).await.body, "b");

    latch.open();
    let answer = scan.await.expect("request");
    assert!(
        !answer.no_match && ["b", "started"].contains(&answer.body.as_str()),
        "every state the scan could have read has a stub, got {answer:?}"
    );
    recorder.write("scan_while_state_moves", roles(1, 1, 1, 0, 0));
    manager.delete_all().await;
}

/// `Scenario.tla`, `NeverDroppedTransition` is violated on purpose: a transition whose gate saw a
/// state that moved loses its compare-and-set, and its answer still goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transition_that_lost_its_compare_and_set_is_dropped() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;

    // Hold one `/go` once its gate read `Started`, before its compare-and-set.
    let at_first_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 0, .. });
    let latch = recorder.hold(at_first_gate);
    let loser = tokio::spawn(async move { get(port, "/go", None).await });
    recorder.reached(at_first_gate).await;

    assert_eq!(get(port, "/go", None).await.body, "started");

    latch.open();
    assert_eq!(loser.await.expect("request").body, "started");
    recorder
        .reached(|p| matches!(p, Point::CasConflict { .. }))
        .await;
    assert_eq!(
        get(port, "/read", None).await.body,
        "a",
        "the winner stands"
    );
    recorder.write("cas_lost", roles(2, 1, 0, 0, 0));
    manager.delete_all().await;
}

/// `Scenario.tla`, `NeverOldAnswerUnderNewStamp` (#1307), on the engine: a request held after its
/// gate read, while a transition lands, serves the old answer under the old stamp, so its client is
/// told about the new answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_overtaken_by_a_transition_keeps_the_stamp_it_copied() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(&manager).await;

    let at_read_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 3, .. });
    let latch = recorder.hold(at_read_gate);
    let old = tokio::spawn(async move { get(port, "/read", None).await });
    recorder.reached(at_read_gate).await;

    assert_eq!(get(port, "/go", None).await.body, "started");
    recorder
        .reached(|p| matches!(p, Point::TransitionStamped { .. }))
        .await;

    latch.open();
    let old = old.await.expect("request");
    assert_eq!(old.body, "started");
    let since = old.last_modified.expect("a conditional stub's stamp");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!((now.status, now.body.as_str()), (200, "a"));
    recorder.write("overtaken_answer", roles(1, 2, 0, 0, 0));
    manager.delete_all().await;
}
