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

async fn create(manager: &ImposterManager, stubs: Value) -> u16 {
    let config = serde_json::from_value(json!({
        "port": 0,
        "host": "127.0.0.1",
        "protocol": "http",
        "stubs": stubs
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
/// and stalls before publishing it; the other two run to the end, one of them serving its answer
/// under the second stamp; then the first publishes, and the map goes back to the lowest stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#1779"]
async fn a_stalled_stamp_does_not_take_the_scenario_back() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/go", Some("Started"), Some("A"), "started"),
            stub("/go", Some("A"), Some("B"), "a"),
            stub("/go", Some("B"), Some("C"), "b"),
            stub("/go", Some("C"), None, "c"),
        ]),
    )
    .await;

    let latch = recorder.hold(|p| matches!(p, Point::TransitionStampIssued { .. }));
    let stalled = tokio::spawn(async move { get(port, "/go", None).await });
    recorder
        .reached(|p| matches!(p, Point::TransitionStampIssued { .. }))
        .await;

    // Issues the second stamp and publishes it; answers `a` under no stamp yet.
    assert_eq!(get(port, "/go", None).await.body, "a");
    // Copies the second stamp, moves the scenario to `C` under the third, and answers `b` under
    // the second. This client now holds `b`.
    let held = get(port, "/go", None).await;
    assert_eq!(held.body, "b");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    assert_eq!(stalled.await.expect("request").body, "started");

    let now = get(port, "/go", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "c"),
        "the client holding `b` under {since} is told about `c`, Last-Modified {:?}",
        now.last_modified
    );
    manager.delete_all().await;
}

/// `Scenario.tla`, `NoStaleAnswerAtRest`: a setter decides "no change" while the state is what it
/// writes, a gated transition moves the state and stamps, and the setter's write moves it back
/// under no stamp at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#1780"]
async fn a_set_that_decided_no_change_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/set", None, Some("B"), "set"),
            stub("/go", Some("B"), Some("C"), "moved"),
            stub("/read", Some("B"), None, "b"),
            stub("/read", Some("C"), None, "c"),
        ]),
    )
    .await;
    assert_eq!(get(port, "/set", None).await.body, "set");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }));
    let setter = tokio::spawn(async move { get(port, "/set", None).await });
    recorder
        .reached(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "moved");
    let held = get(port, "/read", None).await;
    assert_eq!(held.body, "c");
    let since = held.last_modified.expect("a conditional stub's stamp");

    latch.open();
    assert_eq!(setter.await.expect("request").body, "set");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!(
        (now.status, now.body.as_str()),
        (200, "b"),
        "the client holding `c` under {since} is told the state went back to `B`"
    );
    manager.delete_all().await;
}

/// The same window through the admin door: `flow_delete` decides "no change" on an absent key, a
/// gated transition moves the state and stamps, and the delete returns it to `Started` unstamped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#1780"]
async fn a_delete_that_decided_no_change_still_stamps_what_it_moved() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/go", Some("Started"), Some("A"), "moved"),
            stub("/read", Some("Started"), None, "started"),
            stub("/read", Some("A"), None, "a"),
        ]),
    )
    .await;
    let imposter = manager.get_imposter(port).expect("imposter");

    let latch = recorder.hold(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }));
    let flow = port.to_string();
    let delete = tokio::spawn(async move { imposter.flow_delete(&flow, "S") });
    recorder
        .reached(|p| matches!(p, Point::ScenarioChangeRead { changed: false, .. }))
        .await;

    assert_eq!(get(port, "/go", None).await.body, "moved");
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
    manager.delete_all().await;
}

/// `Scenario.tla`, `NoMatchOnlyWhenUncovered`: a scan reads the state once per gated candidate.
/// With the state at `B`, the scan passes the gate of `Started`, then of `A`; a transition takes the
/// state to `Started`; the gate of `B` then fails too. Every state has a stub, and the request
/// matched none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "#1781"]
async fn a_scan_of_covered_states_finds_a_stub_while_the_state_moves() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/read", Some("Started"), None, "started"),
            stub("/read", Some("A"), None, "a"),
            stub("/read", Some("B"), None, "b"),
            stub("/set", None, Some("B"), "set"),
            stub("/go", Some("B"), Some("Started"), "moved"),
        ]),
    )
    .await;
    assert_eq!(get(port, "/set", None).await.body, "set");

    // The gate of the second stub has been read; the third's has not.
    let at_second_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 1, .. });
    let latch = recorder.hold(at_second_gate);
    let scan = tokio::spawn(async move { get(port, "/read", None).await });
    recorder.reached(at_second_gate).await;

    assert_eq!(get(port, "/go", None).await.body, "moved");

    latch.open();
    let answer = scan.await.expect("request");
    assert!(
        !answer.no_match && ["b", "started"].contains(&answer.body.as_str()),
        "every state the scan could have read has a stub, got {answer:?}"
    );
    manager.delete_all().await;
}

/// `Scenario.tla`, `NeverDroppedTransition` is violated on purpose: a transition whose gate saw a
/// state that moved loses its compare-and-set, and its answer still goes out. This one holds today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transition_that_lost_its_compare_and_set_is_dropped() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/go", Some("Started"), Some("A"), "first"),
            stub("/also", Some("Started"), Some("B"), "second"),
            stub("/read", Some("A"), None, "a"),
            stub("/read", Some("B"), None, "b"),
        ]),
    )
    .await;

    // Hold `/go` once its gate read `Started`, before its compare-and-set.
    let at_first_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 0, .. });
    let latch = recorder.hold(at_first_gate);
    let loser = tokio::spawn(async move { get(port, "/go", None).await });
    recorder.reached(at_first_gate).await;

    assert_eq!(get(port, "/also", None).await.body, "second");

    latch.open();
    assert_eq!(loser.await.expect("request").body, "first");
    recorder
        .reached(|p| matches!(p, Point::CasConflict { .. }))
        .await;
    assert_eq!(
        get(port, "/read", None).await.body,
        "b",
        "the winner stands"
    );
    manager.delete_all().await;
}

/// `Scenario.tla`, `NeverOldAnswerUnderNewStamp` (#1307), on the engine: a request held after its
/// gate read, while a transition lands, serves the old answer under the old stamp, so its client
/// is told about the new answer. This one holds today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_overtaken_by_a_transition_keeps_the_stamp_it_copied() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = create(
        &manager,
        json!([
            stub("/go", Some("Started"), Some("A"), "moved"),
            stub("/read", Some("Started"), None, "started"),
            stub("/read", Some("A"), None, "a"),
        ]),
    )
    .await;

    let at_read_gate: Matcher = |p| matches!(p, Point::GateRead { stub_index: 1, .. });
    let latch = recorder.hold(at_read_gate);
    let old = tokio::spawn(async move { get(port, "/read", None).await });
    recorder.reached(at_read_gate).await;

    assert_eq!(get(port, "/go", None).await.body, "moved");
    recorder
        .reached(|p| matches!(p, Point::TransitionStamped { .. }))
        .await;

    latch.open();
    let old = old.await.expect("request");
    assert_eq!(old.body, "started");
    let since = old.last_modified.expect("a conditional stub's stamp");

    let now = get(port, "/read", Some(&since)).await;
    assert_eq!((now.status, now.body.as_str()), (200, "a"));
    manager.delete_all().await;
}
