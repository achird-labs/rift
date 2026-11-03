//! Runs of the imposter lifecycle under a recording probe (`formal/lifecycle`).
//!
//! Each test drives the real `ImposterManager` — creates, deletes, an `apply_config` sweep, a
//! `delete_all`, requests to a conditional stub — some of them concurrently, with a `Probe` that
//! records every lifecycle point in the order the engine took it. With `RIFT_TRACE_DIR` set, each
//! test writes its run as ndjson there, and `formal/lifecycle/validate.py` asks TLC whether
//! `Lifecycle.tla` allows that order. Without it, the recording is checked in memory.
//!
//! The concurrent tests assert only what every interleaving guarantees. The defects the model
//! found (`formal/lifecycle/README.md`, "Findings") are reachable here and are not asserted
//! against: the trace is the evidence, and the fix lands with its own test.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use rift_mock_core::imposter::{ImposterConfig, ImposterManager, Point, Probe};
use serde_json::{Value, json};

/// Records every point with a process-wide sequence number, so points from several tasks sort
/// into the order the engine took them.
#[derive(Default)]
struct Recorder {
    seq: AtomicU64,
    points: Mutex<Vec<(u64, Point)>>,
}

impl Probe for Recorder {
    fn at(&self, point: Point) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.points.lock().push((seq, point));
    }
}

impl Recorder {
    fn points(&self) -> Vec<Point> {
        let mut recorded = self.points.lock().clone();
        recorded.sort_by_key(|(seq, _)| *seq);
        recorded.into_iter().map(|(_, p)| p).collect()
    }

    /// One ndjson line per point: `{"seq", "point", "port", "generation", ...payload}`.
    fn ndjson(&self) -> String {
        let mut recorded = self.points.lock().clone();
        recorded.sort_by_key(|(seq, _)| *seq);
        recorded
            .iter()
            .map(|(seq, point)| {
                let mut line = to_json(*point);
                line["seq"] = json!(seq);
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    /// Write the run to `$RIFT_TRACE_DIR/<name>.ndjson`, when the variable is set.
    fn write(&self, name: &str) {
        let Ok(dir) = std::env::var("RIFT_TRACE_DIR") else {
            return;
        };
        std::fs::create_dir_all(&dir).expect("trace dir");
        std::fs::write(format!("{dir}/{name}.ndjson"), self.ndjson()).expect("write trace");
    }
}

fn to_json(point: Point) -> Value {
    let (name, fields) = match point {
        Point::CreateBound { port, generation } => ("CreateBound", json!({"port": port, "generation": generation})),
        Point::CreateFloorRead { port, generation, floor } => ("CreateFloorRead", json!({"port": port, "generation": generation, "floor": floor})),
        Point::CreateAccepting { port, generation } => ("CreateAccepting", json!({"port": port, "generation": generation})),
        Point::CreateClaimed { port, generation } => ("CreateClaimed", json!({"port": port, "generation": generation})),
        Point::CreateClaimLost { port, generation } => ("CreateClaimLost", json!({"port": port, "generation": generation})),
        Point::CreatePersisted { port, generation } => ("CreatePersisted", json!({"port": port, "generation": generation})),
        Point::CreateRolledBack { port, generation } => ("CreateRolledBack", json!({"port": port, "generation": generation})),
        Point::DeleteLocked { port } => ("DeleteLocked", json!({"port": port})),
        Point::DeleteLookedUp { port, generation } => ("DeleteLookedUp", json!({"port": port, "generation": generation})),
        Point::DeleteNotFound { port } => ("DeleteNotFound", json!({"port": port})),
        Point::DeleteUnlinked { port, generation } => ("DeleteUnlinked", json!({"port": port, "generation": generation})),
        Point::DeleteUnlinkFailed { port, generation } => ("DeleteUnlinkFailed", json!({"port": port, "generation": generation})),
        Point::DeleteFloorRecorded { port, generation, floor } => ("DeleteFloorRecorded", json!({"port": port, "generation": generation, "floor": floor})),
        Point::DeleteRemoved { port, generation } => ("DeleteRemoved", json!({"port": port, "generation": generation})),
        Point::DeleteCancelled { port, generation } => ("DeleteCancelled", json!({"port": port, "generation": generation})),
        Point::DeleteDrained { port, generation, timed_out } => ("DeleteDrained", json!({"port": port, "generation": generation, "timedOut": timed_out})),
        Point::DeleteReturned { port, generation } => ("DeleteReturned", json!({"port": port, "generation": generation})),
        Point::ApplySwept { swept } => ("ApplySwept", json!({"swept": swept})),
        Point::DeleteAllSwept { swept } => ("DeleteAllSwept", json!({"swept": swept})),
        Point::Served { port, generation, stamp } => ("Served", json!({"port": port, "generation": generation, "stamp": stamp})),
        // `Point` is `#[non_exhaustive]`: a point this test does not know is recorded by name.
        other => ("Unknown", json!({"debug": format!("{other:?}")})),
    };
    let mut line = fields;
    line["point"] = json!(name);
    line
}

fn imposter(port: u16) -> ImposterConfig {
    serde_json::from_value(json!({
        "port": port,
        "host": "127.0.0.1",
        "protocol": "http",
        "stubs": [{
            "responses": [{
                "is": { "statusCode": 200, "body": "hello" },
                "_rift": { "conditional": true }
            }]
        }]
    }))
    .expect("imposter config")
}

fn manager(recorder: &Arc<Recorder>) -> ImposterManager {
    let datadir = tempfile::tempdir().expect("datadir");
    // Kept for the manager's lifetime: the directory is deleted with the handle.
    let path = datadir.keep();
    ImposterManager::with_datadir(Some(path)).with_probe(Arc::clone(recorder) as Arc<dyn Probe>)
}

/// A free port: bind port 0, read the port, release it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn get(port: u16) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");
    // A request that lands on no imposter (the port is between generations) is fine: the test
    // records what the engine did, not what the client saw.
    let _ = client.get(format!("http://127.0.0.1:{port}/")).send().await;
}

fn names(points: &[Point]) -> Vec<&'static str> {
    points
        .iter()
        .map(|p| to_json(*p)["point"].as_str().expect("name").to_owned())
        .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_request_delete_in_order() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = manager.create_imposter(imposter(0)).await.expect("create");
    get(port).await;
    manager.delete_imposter(port).await.expect("delete");
    recorder.write("create-request-delete");

    assert_eq!(
        names(&recorder.points()),
        [
            "CreateBound",
            "CreateFloorRead",
            "CreateAccepting",
            "CreateClaimed",
            "CreatePersisted",
            "Served",
            "DeleteLocked",
            "DeleteLookedUp",
            "DeleteUnlinked",
            "DeleteFloorRecorded",
            "DeleteRemoved",
            "DeleteCancelled",
            "DeleteDrained",
            "DeleteReturned",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_creates_race_a_delete_on_one_port() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(manager(&recorder));
    let port = free_port();
    let first = manager.create_imposter(imposter(port)).await.expect("create");
    assert_eq!(first, port);
    get(port).await;

    // The delete goes first; each create waits for the slot to empty, so both reach the bind
    // check once the delete has vacated the port and is cancelling, draining and clearing. Which
    // of the model's interleavings the run then takes is up to the scheduler: one create claims,
    // the other loses the claim or is refused at the check.
    let vacated = || async {
        while manager.get_imposter(port).is_ok() {
            tokio::task::yield_now().await;
        }
    };
    let (d, c1, c2) = tokio::join!(
        manager.delete_imposter(port),
        async {
            vacated().await;
            manager.create_imposter(imposter(port)).await
        },
        async {
            vacated().await;
            manager.create_imposter(imposter(port)).await
        },
    );
    get(port).await;
    recorder.write("race-create-create-delete");

    // Whatever the interleaving: the first delete found its imposter, each later create either
    // won its claim or was refused, and the port holds at most one imposter now.
    assert!(d.is_ok(), "the delete of a registered imposter succeeds: {d:?}");
    for outcome in [&c1, &c2] {
        assert!(
            outcome.is_ok() || matches!(outcome, Err(rift_mock_core::imposter::ImposterError::PortInUse(_))),
            "a racing create wins or is refused: {outcome:?}"
        );
    }
    let holders = manager.get_imposter(port).into_iter().count();
    assert!(holders <= 1);
    let _ = manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_sweep_races_a_create() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(manager(&recorder));
    let port = free_port();
    manager.create_imposter(imposter(port)).await.expect("create");
    get(port).await;

    // An empty desired set sweeps every port; the create races the sweep's delete.
    let (apply, create) = tokio::join!(manager.apply_config(Vec::new()), async {
        tokio::task::yield_now().await;
        manager.create_imposter(imposter(port)).await
    });
    recorder.write("race-apply-sweep-create");

    let report = apply.expect("apply_config");
    assert!(
        report.deleted.contains(&port) || !report.failed.is_empty(),
        "the sweep deleted the port or reported why not: {report:?}"
    );
    assert!(
        create.is_ok() || matches!(create, Err(rift_mock_core::imposter::ImposterError::PortInUse(_))),
        "a racing create wins or is refused: {create:?}"
    );
    let _ = manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_all_races_a_create() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(manager(&recorder));
    let port = free_port();
    manager.create_imposter(imposter(port)).await.expect("create");

    let (_report, create) = tokio::join!(manager.delete_all(), async {
        tokio::task::yield_now().await;
        manager.create_imposter(imposter(port)).await
    });
    get(port).await;
    recorder.write("race-delete-all-create");

    assert!(
        create.is_ok() || matches!(create, Err(rift_mock_core::imposter::ImposterError::PortInUse(_))),
        "a racing create wins or is refused: {create:?}"
    );
    let _ = manager.delete_all().await;
}
