//! Runs of the imposter lifecycle under a recording probe (`formal/lifecycle`).
//!
//! Each test drives the real `ImposterManager` with a `Probe` that records every lifecycle point in
//! the order the engine took it. With `FORMAL_TRACE_DIR` set, a test writes its run as ndjson there,
//! and `formal/lifecycle/validate.py` asks TLC whether `Lifecycle.tla` allows that order.
//!
//! The probe can also hold the engine at a point (`Recorder::hold`), which forces an interleaving
//! instead of hoping the scheduler finds it. Blocking a worker this way is for tests only, so these
//! tests run on a multi-threaded runtime with workers to spare.
//!
//! A test marked `#[ignore = "#n"]` states the correct behaviour and fails today: it reproduces the
//! defect filed as issue n. `cargo test --test lifecycle_interleavings -- --ignored` runs them, and
//! the fix removes the attribute.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar};
use std::time::Duration;

use parking_lot::Mutex;
use rift_mock_core::behaviors::LocalSequencer;
use rift_mock_core::imposter::journal::LocalJournal;
use rift_mock_core::imposter::{ImposterConfig, ImposterError, ImposterManager, Point, Probe};
use rift_mock_core::recording::{
    ClaimOutcome, LocalProxyStore, ProxyMode, ProxyRecordingStore, RequestSignature,
};
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

/// Records every point with a process-wide sequence number, so points from several tasks sort into
/// the order the engine took them, and holds the engine at the points a test asks for.
#[derive(Default)]
struct Recorder {
    seq: AtomicU64,
    points: Mutex<Vec<(u64, Point)>>,
    holds: Mutex<Vec<(Matcher, Arc<Latch>)>>,
}

impl Probe for Recorder {
    fn at(&self, point: Point) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.points.lock().push((seq, point));
        let held = {
            let mut holds = self.holds.lock();
            holds
                .iter()
                .position(|(matches, _)| matches(&point))
                .map(|i| holds.remove(i).1)
        };
        if let Some(latch) = held {
            // Not a plain block: a task the engine has just spawned sits in this worker's LIFO
            // slot, where nothing can steal it. Handing the worker over lets it run during the hold.
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

    fn points(&self) -> Vec<Point> {
        let mut recorded = self.points.lock().clone();
        recorded.sort_by_key(|(seq, _)| *seq);
        recorded.into_iter().map(|(_, p)| p).collect()
    }

    /// Wait until the engine has reached a matching point, and return it.
    async fn reached(&self, matches: Matcher) -> Point {
        for _ in 0..2000 {
            if let Some(point) = self.points().into_iter().find(matches) {
                return point;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the engine never reached the awaited point: {:?}",
            self.points()
        );
    }

    /// One ndjson line per point: `{"seq", "point", "port", "generation", ...payload}`.
    fn ndjson(&self) -> String {
        let mut lines = String::new();
        for (seq, point) in self.points().into_iter().enumerate() {
            let mut line = to_json(point);
            line["seq"] = json!(seq);
            lines.push_str(&line.to_string());
            lines.push('\n');
        }
        lines
    }

    /// Write the run to `$FORMAL_TRACE_DIR/<name>.ndjson`, when the variable is set.
    fn write(&self, name: &str) {
        let Ok(dir) = std::env::var("FORMAL_TRACE_DIR") else {
            return;
        };
        std::fs::create_dir_all(&dir).expect("trace dir");
        std::fs::write(format!("{dir}/{name}.ndjson"), self.ndjson()).expect("write trace");
    }
}

fn to_json(point: Point) -> Value {
    let id = |port: u16, generation: u64| json!({"port": port, "generation": generation});
    let (name, mut line) = match point {
        Point::CreateBound { port, generation } => ("CreateBound", id(port, generation)),
        Point::CreateFloorRead {
            port,
            generation,
            floor,
        } => (
            "CreateFloorRead",
            json!({"port": port, "generation": generation, "floor": floor}),
        ),
        Point::CreateAccepting { port, generation } => ("CreateAccepting", id(port, generation)),
        Point::CreateClaimed { port, generation } => ("CreateClaimed", id(port, generation)),
        Point::CreateClaimLost { port, generation } => ("CreateClaimLost", id(port, generation)),
        Point::CreateFloorReread { port, generation } => {
            ("CreateFloorReread", id(port, generation))
        }
        Point::CreatePersisted { port, generation } => ("CreatePersisted", id(port, generation)),
        Point::CreatePersistFailed { port, generation } => {
            ("CreatePersistFailed", id(port, generation))
        }
        Point::CreateRolledBack { port, generation } => ("CreateRolledBack", id(port, generation)),
        Point::DeleteLocked { port } => ("DeleteLocked", json!({"port": port})),
        Point::DeleteLookedUp { port, generation } => ("DeleteLookedUp", id(port, generation)),
        Point::DeleteNotFound { port } => ("DeleteNotFound", json!({"port": port})),
        Point::DeleteUnlinked { port, generation } => ("DeleteUnlinked", id(port, generation)),
        Point::DeleteUnlinkFailed { port, generation } => {
            ("DeleteUnlinkFailed", id(port, generation))
        }
        Point::DeleteFloorRecorded {
            port,
            generation,
            floor,
        } => (
            "DeleteFloorRecorded",
            json!({"port": port, "generation": generation, "floor": floor}),
        ),
        Point::DeleteRemoved { port, generation } => ("DeleteRemoved", id(port, generation)),
        Point::DeleteCancelled { port, generation } => ("DeleteCancelled", id(port, generation)),
        Point::DeleteCleared { port, generation } => ("DeleteCleared", id(port, generation)),
        Point::DeleteDrained {
            port,
            generation,
            timed_out,
        } => (
            "DeleteDrained",
            json!({"port": port, "generation": generation, "timedOut": timed_out}),
        ),
        Point::DeleteReturned { port, generation } => ("DeleteReturned", id(port, generation)),
        Point::WriteLookedUp { port, generation } => ("WriteLookedUp", id(port, generation)),
        Point::WriteDone { port, generation } => ("WriteDone", id(port, generation)),
        Point::ApplySwept { swept } => ("ApplySwept", json!({"swept": swept})),
        Point::DeleteAllSwept { swept } => ("DeleteAllSwept", json!({"swept": swept})),
        Point::Served {
            port,
            generation,
            stamp,
        } => (
            "Served",
            json!({"port": port, "generation": generation, "stamp": stamp}),
        ),
        // `Point` is `#[non_exhaustive]`: a point this test does not know is recorded by name.
        other => ("Unknown", json!({"debug": format!("{other:?}")})),
    };
    line["point"] = json!(name);
    line
}

fn imposter_with(port: u16, stubs: Value) -> ImposterConfig {
    serde_json::from_value(json!({
        "port": port,
        "host": "127.0.0.1",
        "protocol": "http",
        "recordRequests": true,
        "stubs": stubs
    }))
    .expect("imposter config")
}

/// A conditional stub, so a response carries a `Last-Modified` stamp.
fn answering(port: u16, body: &str) -> ImposterConfig {
    imposter_with(
        port,
        json!([{
            "responses": [{
                "is": { "statusCode": 200, "body": body },
                "_rift": { "conditional": true }
            }]
        }]),
    )
}

/// Two conditional stubs behind a scenario: the first request moves it, so it answers differently
/// from then on, and that transition is the one thing on the request path that stamps.
fn scenario_imposter(port: u16) -> ImposterConfig {
    let stub = |required: &str, next: Option<&str>, body: &str| {
        let mut stub = json!({
            "scenarioName": "S",
            "requiredScenarioState": required,
            "responses": [{
                "is": { "statusCode": 200, "body": body },
                "_rift": { "conditional": true }
            }]
        });
        if let Some(next) = next {
            stub["newScenarioState"] = json!(next);
        }
        stub
    };
    imposter_with(
        port,
        json!([stub("Started", Some("A"), "one"), stub("A", None, "two")]),
    )
}

fn with_probe(recorder: &Arc<Recorder>) -> ImposterManager {
    // Kept for the manager's lifetime: the directory is deleted with the handle.
    let datadir = tempfile::tempdir().expect("datadir").keep();
    ImposterManager::with_datadir(Some(datadir)).with_probe(Arc::clone(recorder) as Arc<dyn Probe>)
}

/// A free port: bind port 0, read the port, release it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Answer {
    status: u16,
    body: String,
    last_modified: Option<String>,
}

/// A request on a fresh connection, so each one is routed to a listener anew.
async fn get(port: u16, if_modified_since: Option<&str>) -> Option<Answer> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let mut request = client.get(format!("http://127.0.0.1:{port}/"));
    if let Some(stamp) = if_modified_since {
        request = request.header("if-modified-since", stamp);
    }
    let response = request.send().await.ok()?;
    let status = response.status().as_u16();
    let last_modified = response
        .headers()
        .get("last-modified")
        .map(|v| v.to_str().expect("header").to_owned());
    let body = response.text().await.ok()?;
    Some(Answer {
        status,
        body,
        last_modified,
    })
}

fn http_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .expect("timestamp")
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn names(points: &[Point]) -> Vec<String> {
    points
        .iter()
        .map(|p| to_json(*p)["point"].as_str().expect("name").to_owned())
        .collect()
}

/// Make `dir` read-only, so a datadir write fails. Root ignores the mode bits, and the tests that
/// rely on this would then fail with a misleading message, so say so here.
#[cfg(unix)]
fn make_read_only(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod datadir");
    assert!(
        std::fs::write(dir.join("write-probe"), b"").is_err(),
        "this test needs a read-only directory to fail writes: run it as a non-root user"
    );
}

#[cfg(unix)]
fn make_writable(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod datadir");
}

fn floor_read(p: &Point) -> bool {
    matches!(p, Point::CreateFloorRead { .. })
}

/// Two creates pass the bind check. The first is held right after it read the port's floor, while
/// the second claims the port, is deleted, and records its floor. Returns the held create's result
/// and the floor it never saw.
async fn create_read_the_floor_before_a_delete(
    recorder: &Arc<Recorder>,
    manager: &Arc<ImposterManager>,
    port: u16,
) -> (Result<u16, ImposterError>, i64) {
    let held = recorder.hold(floor_read);
    let late = tokio::spawn({
        let manager = Arc::clone(manager);
        async move { manager.create_imposter(answering(port, "late")).await }
    });
    recorder.reached(floor_read).await;

    manager
        .create_imposter(answering(port, "earlier"))
        .await
        .expect("the second create passes the bind check and claims");
    manager.delete_imposter(port).await.expect("delete");
    let floor = recorder
        .points()
        .into_iter()
        .find_map(|p| match p {
            Point::DeleteFloorRecorded { floor, .. } => Some(floor),
            _ => None,
        })
        .expect("the delete recorded a floor");

    held.open();
    (late.await.expect("create task"), floor)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_request_delete_in_order() {
    let recorder = Arc::new(Recorder::default());
    let manager = with_probe(&recorder);
    let port = manager
        .create_imposter(answering(0, "hello"))
        .await
        .expect("create");
    get(port, None).await.expect("answered");
    manager.delete_imposter(port).await.expect("delete");
    recorder.write("create-request-delete");

    assert_eq!(
        names(&recorder.points()),
        [
            "CreateBound",
            "CreateFloorRead",
            "CreateAccepting",
            "CreateClaimed",
            "CreateFloorReread",
            "CreatePersisted",
            "Served",
            "DeleteLocked",
            "DeleteLookedUp",
            "DeleteUnlinked",
            "DeleteCancelled",
            "DeleteCleared",
            "DeleteFloorRecorded",
            "DeleteRemoved",
            "DeleteDrained",
            "DeleteReturned",
        ]
    );
}

/// The forced race as a run for trace validation: two generations on one port, the first one's
/// create outliving the second one's whole life. Only what every interleaving guarantees is asserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_outlives_another_generation_on_its_port() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    let (late, _floor) = create_read_the_floor_before_a_delete(&recorder, &manager, port).await;
    assert_eq!(late.expect("the slot is free again"), port);
    get(port, None).await.expect("answered");
    recorder.write("race-create-outlives-generation");
    let _ = manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_sweep_deletes_what_it_saw() {
    let recorder = Arc::new(Recorder::default());
    let manager = with_probe(&recorder);
    let port = free_port();
    manager
        .create_imposter(answering(port, "hello"))
        .await
        .expect("create");
    get(port, None).await.expect("answered");
    let report = manager
        .apply_config(Vec::new())
        .await
        .expect("apply_config");
    recorder.write("apply-sweep");
    assert_eq!(report.deleted, [port], "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_all_deletes_what_it_saw() {
    let recorder = Arc::new(Recorder::default());
    let manager = with_probe(&recorder);
    let port = free_port();
    manager
        .create_imposter(answering(port, "hello"))
        .await
        .expect("create");
    let report = manager.delete_all().await;
    recorder.write("delete-all");
    assert_eq!(report.deleted.len(), 1, "{report:?}");
}

/// #1762, two creates: a create reads the floor before a delete records it, then claims the freed
/// port. A client holding the deleted imposter's stamp must not be told `304` for new content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_racing_a_delete_still_carries_the_floor() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    let (late, floor) = create_read_the_floor_before_a_delete(&recorder, &manager, port).await;
    late.expect("the slot is free again");

    let answer = get(port, Some(&http_date(floor))).await.expect("answered");
    let _ = manager.delete_all().await;
    assert_eq!(
        answer.status, 200,
        "new content under a stamp at or below the deleted imposter's floor {floor}: {:?}",
        answer.last_modified
    );
}

/// #1762: the re-read after the claim only helps if nothing is answered before it. Held there, the
/// imposter holds the port and has not been told the floor yet, so a request must wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_answers_nothing_until_it_has_re_read_the_floor() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();

    let held = recorder.hold(|p| matches!(p, Point::CreateFloorReread { .. }));
    let create = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "new")).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreateFloorReread { .. }))
        .await;

    let early = tokio::time::timeout(Duration::from_millis(500), get(port, None)).await;
    held.open();
    create.await.expect("create task").expect("create");
    let answer = get(port, None).await.map(|a| a.body);
    let _ = manager.delete_all().await;

    assert!(
        early.is_err(),
        "answered before the floor was re-read: {:?}",
        early.map(|a| a.map(|a| a.body))
    );
    assert_eq!(answer.as_deref(), Some("new"));
}

/// #1762, a rollback: a create can answer between its claim and a failed datadir write. Those
/// stamps must carry to the next imposter on the port, as a delete's do.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_still_carries_the_floor() {
    let recorder = Arc::new(Recorder::default());
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let port = free_port();

    make_read_only(&datadir);
    let held = recorder.hold(|p| matches!(p, Point::CreatePersistFailed { .. }));
    let failing = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(scenario_imposter(port)).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreatePersistFailed { .. }))
        .await;

    assert_eq!(get(port, None).await.expect("answered").body, "one");
    let after_transition = get(port, None).await.expect("answered");
    assert_eq!(after_transition.body, "two");
    let stamp = after_transition.last_modified.expect("a stamp");

    held.open();
    assert!(failing.await.expect("create task").is_err());
    make_writable(&datadir);
    manager
        .create_imposter(scenario_imposter(port))
        .await
        .expect("re-create");

    let answer = get(port, Some(&stamp)).await.expect("answered");
    let _ = manager.delete_all().await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (200, "one"),
        "the new imposter's answer differs, so a client holding {stamp} must not get 304"
    );
}

/// #1762, a stamp during the delete: a scenario transition stamps after the delete has begun and
/// before its cancel. The floor is recorded after the cancel, so the next imposter stamps above it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stamp_issued_during_a_delete_still_counts() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    manager
        .create_imposter(scenario_imposter(port))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::DeleteUnlinked { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteUnlinked { .. }))
        .await;

    assert_eq!(get(port, None).await.expect("answered").body, "one");
    let after_transition = get(port, None).await.expect("answered");
    assert_eq!(after_transition.body, "two");
    let stamp = after_transition.last_modified.expect("a stamp");

    held.open();
    delete.await.expect("delete task").expect("delete");
    manager
        .create_imposter(scenario_imposter(port))
        .await
        .expect("re-create");

    let answer = get(port, Some(&stamp)).await.expect("answered");
    let _ = manager.delete_all().await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (200, "one"),
        "the new imposter's answer differs, so a client holding {stamp} must not get 304"
    );
}

/// #1763: a delete clears the shared journal by port, so it must do so before freeing the port. Held
/// after the removal, the next imposter records; the delete finishing must leave that alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_leaves_the_next_imposters_recorded_requests_alone() {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_request_journal(Arc::new(LocalJournal::default())));
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::DeleteDrained { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteDrained { .. }))
        .await;

    manager
        .create_imposter(answering(port, "second"))
        .await
        .expect("the port is free once the slot is vacated");
    assert_eq!(get(port, None).await.expect("answered").body, "second");
    let current = manager.get_imposter(port).expect("registered");
    assert_eq!(current.get_recorded_requests().len(), 1);

    held.open();
    delete.await.expect("delete task").expect("delete");
    let recorded = current.get_recorded_requests().len();
    let _ = manager.delete_all().await;
    assert_eq!(
        recorded, 1,
        "the earlier delete wiped the new imposter's journal"
    );
}

/// #1764: a create that loses the claim must not have answered anyone by the time it is told
/// `PortInUse`, and the winner must have the port to itself afterwards. A connection the kernel
/// queued on the loser's socket stays unanswered, which is why a timeout is allowed in the window
/// and never a "loser" body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_that_loses_the_claim_answers_nobody() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();

    // The winner binds first and waits, so the loser is the later bind on the port.
    let hold_winner = recorder.hold(floor_read);
    let winner = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "winner")).await }
    });
    recorder.reached(floor_read).await;

    let hold_loser = recorder.hold(|p| matches!(p, Point::CreateAccepting { .. }));
    let loser = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "loser")).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreateAccepting { .. }))
        .await;

    hold_winner.open();
    winner
        .await
        .expect("create task")
        .expect("the winner claims");

    // Bounded per request: a connection the kernel queued on a listener nobody serves goes
    // unanswered, and the holds must be released well inside their own bound.
    let mut bodies = Vec::new();
    let mut unanswered = 0;
    for _ in 0..12 {
        match tokio::time::timeout(Duration::from_secs(1), get(port, None)).await {
            Ok(Some(answer)) => bodies.push(answer.body),
            _ => unanswered += 1,
        }
    }
    hold_loser.open();
    let lost = loser.await.expect("create task");

    // The loser's listener closes when its loop sees the shutdown, so a connection can still land
    // on it just after `PortInUse`. Five answers in a row is the winner having the port alone.
    let mut in_a_row = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while in_a_row < 5 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), get(port, None)).await {
            Ok(Some(answer)) => {
                in_a_row += 1;
                bodies.push(answer.body);
            }
            _ => in_a_row = 0,
        }
    }
    let _ = manager.delete_all().await;

    assert!(
        matches!(lost, Err(ImposterError::PortInUse(_))),
        "the later claim loses: {lost:?}"
    );
    assert!(
        !bodies.iter().any(|b| b == "loser"),
        "a create that returned PortInUse answered requests: {bodies:?}, {unanswered} unanswered"
    );
    assert_eq!(
        in_a_row, 5,
        "the winner must answer once the loser has gone: {bodies:?}"
    );
    assert!(
        bodies.iter().all(|b| b == "winner"),
        "every answer comes from the winner: {bodies:?}"
    );
}

/// A replace that loses to a concurrent delete still lands its config: a vacant port is what its
/// delete was for, and reporting a failure would drop what was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replace_that_loses_to_a_delete_still_creates() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    manager
        .create_imposter(answering(port, "old"))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::DeleteUnlinked { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteUnlinked { .. }))
        .await;

    // Decides to replace, since the imposter is still registered, then waits for the lock. A
    // different `defaultResponse` is an imposter-level change, so it replaces rather than patches.
    let mut changed = answering(port, "new");
    changed.default_response =
        Some(serde_json::from_value(json!({ "statusCode": 404 })).expect("default response"));
    let apply = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.apply_one(changed).await }
    });
    // Give it time to reach the lock: holding `apply` proves nothing, as it fires no point before.
    tokio::time::sleep(Duration::from_millis(200)).await;

    held.open();
    delete.await.expect("delete task").expect("delete");
    let report = apply.await.expect("apply task").expect("apply_one");
    let answer = get(port, None).await.map(|a| a.body);
    let _ = manager.delete_all().await;

    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(answer.as_deref(), Some("new"));
}

/// #1763, for the sequencer: the same clear by port, the same imposter that took the port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_leaves_the_next_imposters_response_cycle_alone() {
    let cycling = |port: u16| {
        imposter_with(
            port,
            json!([{ "responses": [
                { "is": { "statusCode": 200, "body": "first" } },
                { "is": { "statusCode": 200, "body": "second" } }
            ] }]),
        )
    };
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_sequencer(Arc::new(LocalSequencer::default())));
    let port = free_port();
    manager
        .create_imposter(cycling(port))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::DeleteDrained { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteDrained { .. }))
        .await;

    manager
        .create_imposter(cycling(port))
        .await
        .expect("the port is free once the slot is vacated");
    assert_eq!(get(port, None).await.expect("answered").body, "first");

    held.open();
    delete.await.expect("delete task").expect("delete");
    let next = get(port, None).await.expect("answered").body;
    let _ = manager.delete_all().await;
    assert_eq!(
        next, "second",
        "the earlier delete restarted the new imposter's cycle"
    );
}

/// `Err` from a delete means "not deleted": the clears come after the unlink's error return, so a
/// failed unlink leaves the imposter serving with everything it had.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_whose_unlink_fails_loses_nothing() {
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = ImposterManager::with_datadir(Some(datadir.clone()))
        .with_request_journal(Arc::new(LocalJournal::default()));
    let port = free_port();
    manager
        .create_imposter(answering(port, "still here"))
        .await
        .expect("create");
    assert_eq!(get(port, None).await.expect("answered").body, "still here");

    make_read_only(&datadir);
    let deleted = manager.delete_imposter(port).await;
    make_writable(&datadir);

    assert!(deleted.is_err(), "the file cannot go, so the delete fails");
    let imposter = manager.get_imposter(port).expect("still registered");
    assert_eq!(
        imposter.get_recorded_requests().len(),
        1,
        "the journal is intact"
    );
    assert_eq!(get(port, None).await.expect("answered").body, "still here");
    manager.delete_imposter(port).await.expect("delete");
}

/// #1773: a failed datadir write rolls back, and by then a delete and a new create may have run.
/// The rollback must remove only its own imposter, or the new one loses its table entry and its
/// listener can never be stopped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_create_rolls_back_only_itself() {
    let recorder = Arc::new(Recorder::default());
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let port = free_port();

    make_read_only(&datadir);
    let held = recorder.hold(|p| matches!(p, Point::CreatePersistFailed { .. }));
    let failing = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "failing")).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreatePersistFailed { .. }))
        .await;

    manager.delete_imposter(port).await.expect("delete");
    make_writable(&datadir);
    manager
        .create_imposter(answering(port, "current"))
        .await
        .expect("create");

    held.open();
    assert!(failing.await.expect("create task").is_err());
    let registered = manager.get_imposter(port).is_ok();
    let answer = get(port, None).await.map(|a| a.body);
    let report = manager.delete_all().await;
    assert!(
        registered,
        "the rollback removed the current imposter from the table; its listener still answers: {answer:?}"
    );
    assert_eq!(answer.as_deref(), Some("current"));
    assert_eq!(
        report.deleted.len(),
        1,
        "a delete still reaches it: {report:?}"
    );
    assert!(
        get(port, None).await.is_none(),
        "nothing answers once the current imposter is deleted"
    );
}

/// The rollback waits for `persist_lock`. A create dropped during that wait, as an admin request
/// whose client went away is, must still roll back: `Err` or nothing, it is not running.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_dropped_while_its_rollback_waits_still_rolls_back() {
    let recorder = Arc::new(Recorder::default());
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let other = manager
        .create_imposter(answering(0, "other"))
        .await
        .expect("create");
    let port = free_port();

    make_read_only(&datadir);
    let write_failed = recorder.hold(|p| matches!(p, Point::CreatePersistFailed { .. }));
    let failing = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "failing")).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreatePersistFailed { .. }))
        .await;

    // A delete elsewhere takes the one `persist_lock` and keeps it.
    let lock_held = recorder.hold(|p| matches!(p, Point::DeleteLocked { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(other).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteLocked { .. }))
        .await;

    write_failed.open();
    // An abort lands at the task's next await, which is the wait for the lock.
    failing.abort();
    assert!(
        failing.await.is_err_and(|e| e.is_cancelled()),
        "the create was dropped, not finished"
    );

    let registered = manager.get_imposter(port).is_ok();
    let answer = get(port, None).await.map(|a| a.body);
    lock_held.open();
    make_writable(&datadir);
    delete.await.expect("delete task").expect("delete");

    assert!(!registered, "a dropped create left its imposter registered");
    assert_eq!(answer, None, "a dropped create left its imposter answering");
}

/// The rollback with nothing racing it: the failed create leaves no imposter behind.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_create_leaves_nothing_registered_or_answering() {
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = ImposterManager::with_datadir(Some(datadir.clone()));
    let port = free_port();

    make_read_only(&datadir);
    let created = manager.create_imposter(answering(port, "failing")).await;
    make_writable(&datadir);

    assert!(created.is_err(), "the write fails: {created:?}");
    assert!(manager.get_imposter(port).is_err(), "not registered");
    assert!(get(port, None).await.is_none(), "not answering");
}

/// #1774: an apply sweep reports a port a concurrent delete already removed as a failure, where
/// `delete_all` treats the same `NotFound` as "already gone, which is what was asked".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apply_sweep_tolerates_a_concurrent_delete() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    manager
        .create_imposter(answering(port, "hello"))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::ApplySwept { .. }));
    let apply = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.apply_config(Vec::new()).await }
    });
    recorder
        .reached(|p| matches!(p, Point::ApplySwept { .. }))
        .await;
    manager.delete_imposter(port).await.expect("delete");
    held.open();

    let report = apply.await.expect("apply task").expect("apply_config");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(
        report.deleted.is_empty(),
        "this apply did not delete it: {report:?}"
    );
    assert!(
        manager.get_imposter(port).is_err(),
        "the port stays vacant, as the apply asked"
    );
}

/// A stub that answers `a`, then `b`, then `a` again.
fn cycling(port: u16) -> ImposterConfig {
    imposter_with(
        port,
        json!([{
            "responses": [
                { "is": { "statusCode": 200, "body": "a" } },
                { "is": { "statusCode": 200, "body": "b" } }
            ]
        }]),
    )
}

fn write_looked_up(p: &Point) -> bool {
    matches!(p, Point::WriteLookedUp { .. })
}

/// #1803: hold `writer` between its lookup and its write, and meanwhile delete the port. Unguarded,
/// the delete runs through during the hold, and `fresh` is created and `prime`d before the writer
/// resumes, so the writer's by-port clear lands on it. Guarded, the delete waits for the writer,
/// and `fresh` is created and primed after both. Either way the caller then checks what `prime`
/// left on `fresh`. Asserts the delete took the lock only after the write was done.
async fn stall_a_write_across_a_recreate<W, P>(
    recorder: &Arc<Recorder>,
    manager: &Arc<ImposterManager>,
    fresh: ImposterConfig,
    writer: W,
    prime: P,
) where
    W: std::future::Future<Output = Result<(), ImposterError>> + Send + 'static,
    P: AsyncFn(),
{
    let port = fresh.port.expect("explicit port");
    let held = recorder.hold(write_looked_up);
    let write = tokio::spawn(writer);
    recorder.reached(write_looked_up).await;
    let delete = tokio::spawn({
        let manager = Arc::clone(manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    // Long enough for a delete that does not wait on the writer to run through.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let raced = delete.is_finished();
    if raced {
        manager
            .create_imposter(fresh.clone())
            .await
            .expect("re-create during the hold");
        prime().await;
    }
    held.open();
    write.await.expect("write task").expect("write");
    delete.await.expect("delete task").expect("delete");
    if !raced {
        manager
            .create_imposter(fresh)
            .await
            .expect("re-create after the delete");
        prime().await;
    }

    let points = names(&recorder.points());
    let done = points.iter().position(|p| p == "WriteDone");
    let locked = points.iter().position(|p| p == "DeleteLocked");
    assert!(
        matches!((done, locked), (Some(done), Some(locked)) if done < locked),
        "the delete took the lock while a write held the port: {points:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_restart_leaves_the_next_imposters_recorded_requests_alone() {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_request_journal(Arc::new(LocalJournal::default())));
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let writer = {
        let manager = Arc::clone(&manager);
        async move { manager.restart_runtime_state(port).await }
    };
    stall_a_write_across_a_recreate(
        &recorder,
        &manager,
        answering(port, "second"),
        writer,
        async || {
            assert_eq!(get(port, None).await.expect("answered").body, "second");
        },
    )
    .await;

    let recorded = manager
        .get_imposter(port)
        .expect("registered")
        .get_recorded_requests()
        .len();
    let _ = manager.delete_all().await;
    assert_eq!(
        recorded, 1,
        "the restart wiped the new imposter's recorded requests"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_saved_requests_clear_leaves_the_next_imposter_alone() {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_request_journal(Arc::new(LocalJournal::default())));
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let writer = {
        let manager = Arc::clone(&manager);
        async move { manager.clear_recorded_requests(port).await }
    };
    stall_a_write_across_a_recreate(
        &recorder,
        &manager,
        answering(port, "second"),
        writer,
        async || {
            assert_eq!(get(port, None).await.expect("answered").body, "second");
        },
    )
    .await;

    let recorded = manager
        .get_imposter(port)
        .expect("registered")
        .get_recorded_requests()
        .len();
    let _ = manager.delete_all().await;
    assert_eq!(
        recorded, 1,
        "the clear wiped the new imposter's recorded requests"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_replace_leaves_the_next_imposters_cursors_alone() {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_sequencer(Arc::new(LocalSequencer::default())));
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let stubs = answering(port, "replaced").stubs;
    let writer = {
        let manager = Arc::clone(&manager);
        async move { manager.replace_stubs(port, stubs).await }
    };
    stall_a_write_across_a_recreate(&recorder, &manager, cycling(port), writer, async || {
        assert_eq!(get(port, None).await.expect("answered").body, "a");
    })
    .await;

    let next = get(port, None).await.expect("answered").body;
    let _ = manager.delete_all().await;
    assert_eq!(next, "b", "the replace restarted the new imposter's cycle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_stub_delete_leaves_the_next_imposters_cursor_alone() {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(with_probe(&recorder).with_sequencer(Arc::new(LocalSequencer::default())));
    let port = free_port();
    // The same stub on both generations, so both key its cursor the same.
    manager
        .create_imposter(cycling(port))
        .await
        .expect("create");

    let writer = {
        let manager = Arc::clone(&manager);
        async move { manager.delete_stub(port, 0).await }
    };
    stall_a_write_across_a_recreate(&recorder, &manager, cycling(port), writer, async || {
        assert_eq!(get(port, None).await.expect("answered").body, "a");
    })
    .await;

    let next = get(port, None).await.expect("answered").body;
    let _ = manager.delete_all().await;
    assert_eq!(next, "b", "the stub delete reset the new imposter's cursor");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_proxy_clear_leaves_the_next_imposters_recordings_alone() {
    let recorder = Arc::new(Recorder::default());
    let store = Arc::new(LocalProxyStore::new(ProxyMode::ProxyOnce));
    let manager = Arc::new(
        with_probe(&recorder).with_proxy_store(Arc::clone(&store) as Arc<dyn ProxyRecordingStore>),
    );
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let signature = RequestSignature::new("GET", "/recorded", None, &[]);
    let writer = {
        let manager = Arc::clone(&manager);
        async move { manager.clear_proxy_responses(port).await }
    };
    stall_a_write_across_a_recreate(
        &recorder,
        &manager,
        answering(port, "second"),
        writer,
        async || {
            // The new imposter's proxy takes a claim in the shared store.
            assert!(matches!(
                store.try_claim(port, &signature).expect("claim"),
                ClaimOutcome::Claimed(_)
            ));
        },
    )
    .await;

    let again = store.try_claim(port, &signature).expect("claim");
    let _ = manager.delete_all().await;
    assert!(
        matches!(again, ClaimOutcome::InFlight),
        "the proxy clear wiped the new imposter's claim: {again:?}"
    );
}

/// #1803: an apply that looked the imposter up before a delete must not patch it, or reset the port's
/// cursors, once the delete has vacated the port: it reports the port as not found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apply_whose_imposter_was_deleted_reports_it_not_found() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(with_probe(&recorder));
    let port = free_port();
    manager
        .create_imposter(answering(port, "first"))
        .await
        .expect("create");

    let held = recorder.hold(|p| matches!(p, Point::DeleteLocked { .. }));
    let delete = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.delete_imposter(port).await.map(|_| ()) }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteLocked { .. }))
        .await;
    // Only the stubs differ, so the apply takes the in-place patch and waits on the delete's lock.
    let apply = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.apply_one(answering(port, "patched")).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    held.open();
    delete.await.expect("delete task").expect("delete");
    let report = apply.await.expect("apply task").expect("apply");

    assert!(report.stub_patched.is_empty(), "{report:?}");
    assert!(
        matches!(report.failed.as_slice(), [(p, ImposterError::NotFound(q))] if *p == port && *q == port),
        "{report:?}"
    );
    assert!(manager.get_imposter(port).is_err());
}

/// #1800: a replace that loses its port to a concurrent create leaves `{port}.json` for that create
/// to overwrite. When the create's own write fails and it rolls back, the port is vacant and the file
/// still holds the imposter the replace reported deleted, so a restart would bring it back. The
/// create that took the port owns the file: its rollback removes it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replace_that_loses_its_port_to_a_failed_create_leaves_no_file() {
    let recorder = Arc::new(Recorder::default());
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let port = free_port();
    manager
        .create_imposter(answering(port, "old"))
        .await
        .expect("create");
    let file = datadir.join(format!("{port}.json"));
    assert!(file.exists());

    make_read_only(&datadir);
    // An imposter-level change, so the apply replaces: delete without unlinking, then create.
    let mut renamed = answering(port, "replaced");
    renamed.name = Some("renamed".to_owned());
    // Held after the slot is vacated and `persist_lock` released, before its own create.
    let replace_held = recorder.hold(|p| matches!(p, Point::DeleteDrained { .. }));
    let replace = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.apply_one(renamed).await }
    });
    recorder
        .reached(|p| matches!(p, Point::DeleteDrained { .. }))
        .await;

    let create_held = recorder.hold(|p| matches!(p, Point::CreatePersistFailed { .. }));
    let create = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.create_imposter(answering(port, "c2")).await }
    });
    recorder
        .reached(|p| matches!(p, Point::CreatePersistFailed { .. }))
        .await;

    replace_held.open();
    let report = replace.await.expect("replace task").expect("apply");
    assert!(report.deleted.contains(&port), "{report:?}");
    assert!(
        report
            .failed
            .iter()
            .any(|(p, e)| *p == port && matches!(e, ImposterError::PortInUse(_))),
        "{report:?}"
    );

    make_writable(&datadir);
    create_held.open();
    assert!(create.await.expect("create task").is_err());
    assert!(manager.get_imposter(port).is_err());
    assert!(
        !file.exists(),
        "the replaced imposter's file survived with the port vacant, so a restart brings it back"
    );
}

/// #1800, over-fix guard: a create from a datadir file is that file's own load, and a failed write
/// must leave the file as it was (#1158). The datadir is made writable again before the rollback, so
/// an unlink there would succeed: only `RollbackFile::Keep` keeps the file.
#[cfg(unix)]
async fn a_datadir_create_whose_write_fails_keeps_its_file(reload: bool) {
    let recorder = Arc::new(Recorder::default());
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let port = free_port();
    let file = datadir.join(format!("{port}.json"));
    std::fs::write(&file, b"{}").expect("seed the file");

    make_read_only(&datadir);
    let held = recorder.hold(|p| matches!(p, Point::CreatePersistFailed { .. }));
    let load = tokio::spawn({
        let manager = Arc::clone(&manager);
        let scripts = rift_mock_core::imposter::ScriptFileAccess::Confined(std::env::temp_dir());
        async move {
            let config = answering(port, "loaded");
            if reload {
                let report = manager
                    .apply_desired(vec![rift_mock_core::imposter::DesiredImposter {
                        config,
                        persistence: rift_mock_core::imposter::Persistence::Datadir,
                        script_files: scripts,
                    }])
                    .await
                    .expect("apply");
                assert!(report.created.is_empty(), "{report:?}");
            } else {
                assert!(
                    manager
                        .create_imposter_resuming(config, scripts)
                        .await
                        .is_err()
                );
            }
        }
    });
    recorder
        .reached(|p| matches!(p, Point::CreatePersistFailed { .. }))
        .await;
    make_writable(&datadir);
    held.open();
    load.await.expect("load task");

    assert!(manager.get_imposter(port).is_err());
    assert_eq!(
        std::fs::read(&file).ok().as_deref(),
        Some(&b"{}"[..]),
        "a datadir load's rollback removed its own input file"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_startup_load_whose_write_fails_keeps_its_file() {
    a_datadir_create_whose_write_fails_keeps_its_file(false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_whose_write_fails_keeps_its_file() {
    a_datadir_create_whose_write_fails_keeps_its_file(true).await;
}

/// #1800: a create whose write fails on a port with no file rolls back with its own write error,
/// not a cleanup error.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_create_with_no_file_reports_its_write_error() {
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = ImposterManager::with_datadir(Some(datadir.clone()));
    let port = free_port();
    make_read_only(&datadir);
    let created = manager.create_imposter(answering(port, "fails")).await;
    make_writable(&datadir);
    // The write's own error, not the cleanup's (`Failed to remove imposter …`).
    assert!(
        matches!(&created, Err(ImposterError::PersistError(e)) if !format!("{e:#}").contains("Failed to remove")),
        "{created:?}"
    );
    assert!(manager.get_imposter(port).is_err());
    assert!(!datadir.join(format!("{port}.json")).exists());
}
