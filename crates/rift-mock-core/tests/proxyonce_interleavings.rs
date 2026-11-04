//! Runs of the proxyOnce claim protocol under a recording probe (`formal/proxyonce`).
//!
//! Each test drives a real imposter, proxying to a second one, with a `Probe` that records every
//! claim-protocol point in the order the engine took it, and can hold the engine at a point
//! (`Recorder::hold`), which forces an interleaving instead of hoping the scheduler finds it. With
//! `FORMAL_TRACE_DIR` set, a test writes its run as ndjson there, and `formal/proxyonce/validate.py`
//! asks TLC whether `ProxyOnce.tla` allows that order.
//!
//! A test marked `#[ignore = "#n"]` states the correct behaviour and fails today: it reproduces the
//! defect filed as issue n. `cargo test --test proxyonce_interleavings -- --ignored` runs them, and
//! the fix removes the attribute.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar};
use std::time::Duration;

use parking_lot::Mutex;
use rift_mock_core::imposter::{Imposter, ImposterConfig, ImposterManager, Point, Probe};
use rift_mock_core::recording::{
    ClaimOutcome, ClaimToken, LocalProxyStore, ProxyMode, ProxyRecordingStore, ProxyStoreError,
    RecordedResponse, RequestSignature,
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

/// Records every point with a process-wide sequence number, and holds the engine at the points a
/// test asks for.
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

    /// Wait until the engine has reached a matching point.
    async fn reached(&self, matches: Matcher) {
        for _ in 0..2000 {
            if self.points().iter().any(matches) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "the engine never reached the awaited point: {:?}",
            self.points()
        );
    }

    /// Forget what was recorded so far, so `reached` only sees what happens from here.
    fn restart(&self) {
        self.points.lock().clear();
    }

    /// Write the run to `$FORMAL_TRACE_DIR/<name>.ndjson`, when the variable is set.
    ///
    /// The `Start` line says whether the run starts with a recording made before `restart`.
    fn write(&self, name: &str, preloaded: bool) {
        let Ok(dir) = std::env::var("FORMAL_TRACE_DIR") else {
            return;
        };
        let mut lines = json!({"point": "Start", "preloaded": preloaded}).to_string();
        lines.push('\n');
        for (seq, point) in self.points().into_iter().enumerate() {
            let mut line = to_json(point);
            line["seq"] = json!(seq);
            lines.push_str(&line.to_string());
            lines.push('\n');
        }
        std::fs::create_dir_all(&dir).expect("trace dir");
        std::fs::write(format!("{dir}/{name}.ndjson"), lines).expect("write trace");
    }
}

fn to_json(point: Point) -> Value {
    match point {
        Point::ProxyClaim { outcome, .. } => {
            let (outcome, token) = match outcome {
                ClaimOutcome::Claimed(token) => ("Claimed", Some(token.value())),
                ClaimOutcome::InFlight => ("InFlight", None),
                ClaimOutcome::AlreadyRecorded => ("AlreadyRecorded", None),
            };
            json!({"point": "ProxyClaim", "outcome": outcome, "token": token})
        }
        Point::ProxyShadowed { .. } => json!({"point": "ProxyShadowed"}),
        Point::ProxyShadowForgot { .. } => json!({"point": "ProxyShadowForgot"}),
        Point::ProxyLookingUp { .. } => json!({"point": "ProxyLookingUp"}),
        Point::ProxyReplayed { .. } => json!({"point": "ProxyReplayed"}),
        Point::ProxyNothingToReplay { .. } => json!({"point": "ProxyNothingToReplay"}),
        Point::ProxyForwarding { .. } => json!({"point": "ProxyForwarding"}),
        Point::ProxySettled { .. } => json!({"point": "ProxySettled"}),
        Point::ProxyInserted { claimed, .. } => {
            json!({"point": "ProxyInserted", "claimed": claimed})
        }
        Point::ProxyOrphanFound { .. } => json!({"point": "ProxyOrphanFound"}),
        Point::ProxyOrphanForgotten { .. } => json!({"point": "ProxyOrphanForgotten"}),
        Point::ProxyStoreCleared { .. } => json!({"point": "ProxyStoreCleared"}),
        // Lifecycle points: not this model's.
        other => json!({"point": "Other", "debug": format!("{other:?}")}),
    }
}

fn shadowed(p: &Point) -> bool {
    matches!(p, Point::ProxyShadowed { .. })
}
fn looking_up(p: &Point) -> bool {
    matches!(p, Point::ProxyLookingUp { .. })
}
fn forwarding(p: &Point) -> bool {
    matches!(p, Point::ProxyForwarding { .. })
}
fn settled(p: &Point) -> bool {
    matches!(p, Point::ProxySettled { .. })
}
fn orphan_found(p: &Point) -> bool {
    matches!(p, Point::ProxyOrphanFound { .. })
}

/// A proxyOnce imposter in front of an upstream one that counts what it is asked.
struct Rig {
    recorder: Arc<Recorder>,
    store: Arc<LocalProxyStore>,
    /// Kept alive: it owns both imposters.
    _manager: ImposterManager,
    imposter: Arc<Imposter>,
    upstream: Arc<Imposter>,
    port: u16,
}

impl Rig {
    async fn new() -> Self {
        let recorder = Arc::new(Recorder::default());
        let store = Arc::new(LocalProxyStore::new(ProxyMode::ProxyOnce));
        let manager = ImposterManager::new()
            .with_proxy_store(Arc::clone(&store) as Arc<dyn ProxyRecordingStore>)
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>);
        let upstream_port = manager
            .create_imposter(config(
                0,
                json!([{ "responses": [{ "is": { "statusCode": 200, "body": "up" } }] }]),
            ))
            .await
            .expect("upstream");
        // addWaitBehavior generates a stub with no predicates, so the claim keys on the request.
        let port = manager
            .create_imposter(config(
                0,
                json!([{ "responses": [{ "proxy": {
                    "to": format!("http://127.0.0.1:{upstream_port}"),
                    "mode": "proxyOnce",
                    "addWaitBehavior": true,
                } }] }]),
            ))
            .await
            .expect("proxy");
        Self {
            recorder,
            store,
            imposter: manager.get_imposter(port).expect("proxy imposter"),
            upstream: manager
                .get_imposter(upstream_port)
                .expect("upstream imposter"),
            _manager: manager,
            port,
        }
    }

    /// The first request: `[recording, proxy]`, and the recorder starts over.
    async fn recorded(&self) {
        get(self.port).await;
        assert_eq!(self.layout(), ["recording", "proxy"]);
        self.recorder.restart();
    }

    /// `[proxy, recording]`: the recording is shadowed, and a request reaching the proxy misses it.
    async fn shadowed(&self) {
        self.recorded().await;
        self.imposter.move_stub(0, 1).expect("move");
        assert_eq!(self.layout(), ["proxy", "recording"]);
    }

    fn layout(&self) -> Vec<&'static str> {
        self.imposter
            .get_stubs()
            .iter()
            .map(|stub| {
                if stub.responses.iter().any(|r| r.is_recorded()) {
                    "recording"
                } else {
                    "proxy"
                }
            })
            .collect()
    }

    /// The recordings a request can still be answered by.
    fn ahead_of_the_proxy(&self) -> usize {
        self.layout()
            .iter()
            .take_while(|kind| **kind == "recording")
            .count()
    }

    fn upstream_hits(&self) -> usize {
        self.upstream.get_recorded_requests().len()
    }

    /// Whether the store holds a recording for the one request these tests send.
    fn store_has_recording(&self) -> bool {
        let sig = RequestSignature::new("GET", "/", None, &[]).with_body(&[]);
        self.store.lookup(self.port, &sig).is_some()
    }

    fn inserted_without_a_claim(&self) -> bool {
        self.recorder
            .points()
            .iter()
            .any(|p| matches!(p, Point::ProxyInserted { claimed: false, .. }))
    }

    fn spawn_get(&self) -> tokio::task::JoinHandle<String> {
        tokio::spawn(get(self.port))
    }
}

fn config(port: u16, stubs: Value) -> ImposterConfig {
    serde_json::from_value(json!({
        "port": port,
        "host": "127.0.0.1",
        "protocol": "http",
        "recordRequests": true,
        "stubs": stubs
    }))
    .expect("imposter config")
}

/// A request on a fresh connection; its body.
async fn get(port: u16) -> String {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(40))
        .build()
        .expect("client");
    client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body")
}

// Runs the model allows, with no hold: `formal/proxyonce/validate.py` checks their traces.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_answers_the_next_request_from_its_stub() {
    let rig = Rig::new().await;
    get(rig.port).await;
    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy"]);
    assert_eq!(rig.upstream_hits(), 1);
    assert!(rig.store_has_recording());
    rig.recorder
        .write("record_then_answer_from_the_stub", false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_first_request_forwards_and_does_not_record() {
    let rig = Rig::new().await;
    let held = rig.recorder.hold(forwarding);
    let winner = rig.spawn_get();
    rig.recorder.reached(forwarding).await;

    assert_eq!(get(rig.port).await, "up", "the loser forwards");
    assert_eq!(rig.layout(), ["proxy"], "the loser inserts nothing");

    held.open();
    winner.await.expect("winner");
    assert_eq!(rig.layout(), ["recording", "proxy"]);
    assert_eq!(rig.upstream_hits(), 2);
    assert!(!rig.inserted_without_a_claim());
    rig.recorder.write("loser_forwards_unrecorded", false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_matched_before_the_recording_landed_replays_it() {
    let rig = Rig::new().await;
    // Held in the upstream await, so the second request matches the proxy stub and claims after
    // the first settled: it is held at the settle, before the insert.
    let held = rig.recorder.hold(settled);
    let first = rig.spawn_get();
    rig.recorder.reached(settled).await;

    assert_eq!(get(rig.port).await, "up", "replayed from the store");
    assert_eq!(rig.upstream_hits(), 1);

    held.open();
    first.await.expect("first");
    assert_eq!(rig.layout(), ["recording", "proxy"]);
    rig.recorder.write("replay_before_the_stub_landed", false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_recording_forgets_it_and_the_next_request_records_again() {
    let rig = Rig::new().await;
    rig.recorded().await;
    rig.imposter.delete_stub(0).expect("delete");
    assert!(!rig.store_has_recording());

    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy"]);
    assert_eq!(rig.upstream_hits(), 2);
    rig.recorder.write("delete_then_record_again", true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shadowed_recording_is_forwarded_past_and_recorded_again() {
    let rig = Rig::new().await;
    rig.shadowed().await;

    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy", "recording"]);
    assert_eq!(rig.upstream_hits(), 2);
    assert!(rig.store_has_recording());
    rig.recorder.write("shadow_forward_and_record_again", true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clearing_saved_responses_clears_the_recording_and_its_stub() {
    let rig = Rig::new().await;
    rig.recorded().await;
    let _ = rig.imposter.clear_proxy_responses();
    assert_eq!(rig.layout(), ["proxy"]);
    assert!(!rig.store_has_recording());
    rig.recorder.write("clear_then_nothing", true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_window_between_an_edit_and_its_forget_replays_the_recording() {
    let rig = Rig::new().await;
    rig.recorded().await;
    let held = rig.recorder.hold(orphan_found);
    let imposter = Arc::clone(&rig.imposter);
    let edit = tokio::spawn(async move { imposter.delete_stub(0) });
    rig.recorder.reached(orphan_found).await;

    assert_eq!(get(rig.port).await, "up", "replayed from the store");
    assert_eq!(rig.upstream_hits(), 1);

    held.open();
    edit.await.expect("edit").expect("delete");
    assert!(!rig.store_has_recording());
    rig.recorder.write("replay_in_the_forget_window", true);
}

// Each of these reproduced a filed defect, now fixed.

/// Two requests miss a shadowed recording together. The first forgets it and records anew; the
/// second, which decided before that, forgets the *new* recording and records again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_shadow_forget_leaves_the_new_recording_alone() {
    let rig = Rig::new().await;
    rig.shadowed().await;

    let held = rig.recorder.hold(shadowed);
    let late = rig.spawn_get();
    rig.recorder.reached(shadowed).await;

    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy", "recording"]);
    assert_eq!(rig.upstream_hits(), 2);

    held.open();
    late.await.expect("late");
    rig.recorder
        .write("late_shadow_forget_leaves_the_new_recording", true);
    assert_eq!(
        rig.ahead_of_the_proxy(),
        1,
        "one identity, one recording ahead of the proxy: {:?}",
        rig.layout()
    );
    assert_eq!(
        rig.upstream_hits(),
        2,
        "the late request had a recording to replay"
    );
}

/// A request that finds a recording settled and not yet inserted, with the old carrier still behind
/// the proxy, takes the new recording for the shadowed one and forgets it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_recording_is_not_taken_for_the_shadowed_one() {
    let rig = Rig::new().await;
    rig.shadowed().await;

    let held = rig.recorder.hold(settled);
    let first = rig.spawn_get();
    rig.recorder.reached(settled).await;
    assert_eq!(rig.upstream_hits(), 2);

    get(rig.port).await;

    held.open();
    first.await.expect("first");
    rig.recorder
        .write("new_recording_not_taken_for_shadowed", true);
    assert_eq!(
        rig.ahead_of_the_proxy(),
        1,
        "one identity, one recording ahead of the proxy: {:?}",
        rig.layout()
    );
    assert_eq!(
        rig.upstream_hits(),
        2,
        "the second request had a recording to replay"
    );
}

/// An edit removes the old carrier of a shadowed recording while a new recording is on its way. Its
/// forget, decided from the list it swapped, removes the new recording, whose stub is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edits_forget_leaves_a_recording_with_a_live_stub_alone() {
    let rig = Rig::new().await;
    rig.shadowed().await;

    let held_request = rig.recorder.hold(settled);
    let request = rig.spawn_get();
    rig.recorder.reached(settled).await;

    let held_edit = rig.recorder.hold(orphan_found);
    let imposter = Arc::clone(&rig.imposter);
    let edit = tokio::spawn(async move { imposter.delete_stub(1) });
    rig.recorder.reached(orphan_found).await;

    held_request.open();
    request.await.expect("request");
    assert_eq!(rig.layout(), ["recording", "proxy"]);

    held_edit.open();
    edit.await.expect("edit").expect("delete");
    rig.recorder
        .write("edit_forget_leaves_the_live_recording", true);
    assert!(
        rig.store_has_recording(),
        "the recording's stub is in the list, so the recording stays"
    );
}

/// A request in flight across a savedProxyResponses clear holds a token the store has forgotten.
/// `[proxy, T1, T2]` with the entry at `T2`: every recording is behind the proxy stub, so the request
/// must forward. Forgetting only `T1`, the first carrier, would be a no-op and the request would
/// replay a shadowed recording forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_behind_an_older_carrier_is_still_forwarded_past() {
    let rig = Rig::new().await;
    rig.shadowed().await;
    assert_eq!(get(rig.port).await, "up");
    assert_eq!(rig.layout(), ["recording", "proxy", "recording"]);
    assert_eq!(rig.upstream_hits(), 2);
    rig.imposter.move_stub(0, 2).expect("move");
    assert_eq!(rig.layout(), ["proxy", "recording", "recording"]);

    assert_eq!(get(rig.port).await, "up");
    assert_eq!(rig.upstream_hits(), 3, "it forwarded rather than replayed");
    assert_eq!(
        rig.layout(),
        ["recording", "proxy", "recording", "recording"]
    );
}

/// `[T2, proxy, T1]` with the entry at `T2`: deleting `T1` removes a recording the store no longer
/// holds, and must leave `T2`'s entry alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_the_older_carrier_keeps_the_newer_recording() {
    let rig = Rig::new().await;
    rig.shadowed().await;
    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy", "recording"]);

    rig.imposter.delete_stub(2).expect("delete");
    assert!(rig.store_has_recording(), "T2's entry stays");
    assert_eq!(get(rig.port).await, "up");
    assert_eq!(rig.upstream_hits(), 2, "T2 answers");
}

/// `[T2, proxy, T1]` with the entry at `T2`: deleting `T2` deletes the recording the store holds, so
/// the identity records anew, though `T1` still carries the same signature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_the_newer_carrier_forgets_its_recording_despite_an_older_one() {
    let rig = Rig::new().await;
    rig.shadowed().await;
    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy", "recording"]);

    rig.imposter.delete_stub(0).expect("delete");
    assert!(!rig.store_has_recording(), "T2's entry went with its stub");
}

/// A clear stales the request's claim, so the store drops its recording. It must leave no stub
/// either, or the stub would stack next to the recording a later request made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_the_store_dropped_inserts_no_stub() {
    let rig = Rig::new().await;

    let held = rig.recorder.hold(forwarding);
    let stale = rig.spawn_get();
    rig.recorder.reached(forwarding).await;

    let _ = rig.imposter.clear_proxy_responses();

    get(rig.port).await;
    assert_eq!(rig.layout(), ["recording", "proxy"]);

    held.open();
    stale.await.expect("stale");
    rig.recorder.write("stale_token_inserts_nothing", false);
    assert_eq!(
        rig.ahead_of_the_proxy(),
        1,
        "one identity, one recording ahead of the proxy: {:?}",
        rig.layout()
    );
}

/// A request's `AlreadyRecorded` is followed by an edit's forget, so its lookup finds nothing. It
/// claims again and records with that claim: a stub with no claim would be invisible to the store's
/// bookkeeping, and a second request would stack another on top of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_lookup_claims_again_and_records() {
    let rig = Rig::new().await;
    rig.recorded().await;

    let held_edit = rig.recorder.hold(orphan_found);
    let imposter = Arc::clone(&rig.imposter);
    let edit = tokio::spawn(async move { imposter.delete_stub(0) });
    rig.recorder.reached(orphan_found).await;

    let held_request = rig.recorder.hold(looking_up);
    let request = rig.spawn_get();
    rig.recorder.reached(looking_up).await;

    held_edit.open();
    edit.await.expect("edit").expect("delete");

    held_request.open();
    assert_eq!(request.await.expect("request"), "up");
    rig.recorder.write("reclaim_after_nothing_to_replay", true);
    assert!(
        !rig.inserted_without_a_claim(),
        "points: {:?}",
        rig.recorder.points()
    );
    assert_eq!(rig.layout(), ["recording", "proxy"]);
    assert!(rig.store_has_recording(), "the claim's recording landed");
    assert_eq!(rig.upstream_hits(), 2, "the first request and this one");
    assert_eq!(get(rig.port).await, "up");
    assert_eq!(rig.upstream_hits(), 2, "the next request replays");
}

/// As above, but another request holds the claim when this one tries again: it lost the race, so it
/// forwards and leaves the recording to the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_lookup_that_finds_the_claim_taken_inserts_no_stub() {
    let rig = Rig::new().await;
    rig.recorded().await;

    let held_edit = rig.recorder.hold(orphan_found);
    let imposter = Arc::clone(&rig.imposter);
    let edit = tokio::spawn(async move { imposter.delete_stub(0) });
    rig.recorder.reached(orphan_found).await;

    let held_request = rig.recorder.hold(looking_up);
    let request = rig.spawn_get();
    rig.recorder.reached(looking_up).await;

    held_edit.open();
    edit.await.expect("edit").expect("delete");
    let sig = RequestSignature::new("GET", "/", None, &[]).with_body(&[]);
    assert!(matches!(
        rig.store.try_claim_as(rig.port, &sig, ProxyMode::ProxyOnce),
        Ok(ClaimOutcome::Claimed(_))
    ));

    held_request.open();
    assert_eq!(request.await.expect("request"), "up");
    assert_eq!(rig.layout(), ["proxy"], "the claim's holder records");
    assert_eq!(rig.upstream_hits(), 2, "it still forwarded");
}

/// A store that answers `AlreadyRecorded` and then has nothing to replay, whatever it is asked.
#[derive(Default)]
struct HollowStore {
    claims: AtomicU64,
}

impl ProxyRecordingStore for HollowStore {
    fn try_claim(&self, _: u16, _: &RequestSignature) -> Result<ClaimOutcome, ProxyStoreError> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Ok(ClaimOutcome::AlreadyRecorded)
    }
    fn release_claim(&self, _: u16, _: &RequestSignature, _: ClaimToken) {}
    fn record(
        &self,
        _: u16,
        _: RequestSignature,
        _: ClaimToken,
        _: RecordedResponse,
    ) -> Result<(), ProxyStoreError> {
        Ok(())
    }
    fn lookup(&self, _: u16, _: &RequestSignature) -> Option<RecordedResponse> {
        None
    }
    fn clear(&self, _: u16) {}
}

/// A manager with an upstream imposter and a generating proxy in front of it, on `store`.
async fn proxying_through(
    store: Arc<dyn ProxyRecordingStore>,
    mode: &str,
) -> (ImposterManager, u16, Arc<Imposter>) {
    let manager = ImposterManager::new().with_proxy_store(store);
    let upstream_port = manager
        .create_imposter(config(
            0,
            json!([{ "responses": [{ "is": { "statusCode": 200, "body": "up" } }] }]),
        ))
        .await
        .expect("upstream");
    let port = manager
        .create_imposter(config(
            0,
            json!([{ "responses": [{ "proxy": {
                "to": format!("http://127.0.0.1:{upstream_port}"),
                "mode": mode,
                "addWaitBehavior": true,
            } }] }]),
        ))
        .await
        .expect("proxy");
    let upstream = manager.get_imposter(upstream_port).expect("upstream");
    (manager, port, upstream)
}

/// Claiming again is bounded: a store that keeps saying `AlreadyRecorded` with nothing behind it
/// gets two claims, the request is forwarded, and no stub lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_with_nothing_behind_its_answer_is_asked_twice() {
    let store = Arc::new(HollowStore::default());
    let (manager, port, _) = proxying_through(
        Arc::clone(&store) as Arc<dyn ProxyRecordingStore>,
        "proxyOnce",
    )
    .await;

    assert_eq!(get(port).await, "up");
    assert_eq!(store.claims.load(Ordering::SeqCst), 2);
    let stubs = manager.get_imposter(port).expect("proxy").get_stubs();
    assert_eq!(stubs.len(), 1, "no stub landed: {stubs:?}");
}

/// A store that grants real claims and answers every settle with what it is told to.
struct SettlingStore {
    inner: LocalProxyStore,
    fails: bool,
}

impl SettlingStore {
    fn new(fails: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: LocalProxyStore::new(ProxyMode::ProxyOnce),
            fails,
        })
    }
}

impl ProxyRecordingStore for SettlingStore {
    fn try_claim(
        &self,
        port: u16,
        sig: &RequestSignature,
    ) -> Result<ClaimOutcome, ProxyStoreError> {
        self.inner.try_claim(port, sig)
    }
    fn try_claim_as(
        &self,
        port: u16,
        sig: &RequestSignature,
        mode: ProxyMode,
    ) -> Result<ClaimOutcome, ProxyStoreError> {
        self.inner.try_claim_as(port, sig, mode)
    }
    fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
        self.inner.release_claim(port, sig, token);
    }
    fn record(
        &self,
        _: u16,
        _: RequestSignature,
        _: ClaimToken,
        _: RecordedResponse,
    ) -> Result<(), ProxyStoreError> {
        if self.fails {
            Err(ProxyStoreError::Unavailable("settle fails".into()))
        } else {
            Ok(())
        }
    }
    fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
        self.inner.lookup(port, sig)
    }
    fn clear(&self, port: u16) {
        self.inner.clear(port);
    }
}

/// A failed settle promises the signature stays retryable (#315), which a stub answering every
/// later request would break.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_settle_inserts_no_stub_and_the_next_request_forwards() {
    let store = SettlingStore::new(true);
    let (manager, port, upstream) = proxying_through(
        Arc::clone(&store) as Arc<dyn ProxyRecordingStore>,
        "proxyOnce",
    )
    .await;
    assert_eq!(get(port).await, "up");
    assert_eq!(get(port).await, "up");
    let stubs = manager.get_imposter(port).expect("proxy").get_stubs();
    assert_eq!(stubs.len(), 1, "no stub landed: {stubs:?}");
    assert_eq!(upstream.get_recorded_requests().len(), 2);
    // A claim still pending would answer `InFlight`, and the second request would forward too.
    let sig = RequestSignature::new("GET", "/", None, &[]).with_body(&[]);
    assert!(
        matches!(store.try_claim(port, &sig), Ok(ClaimOutcome::Claimed(_))),
        "the failed settle released its claim"
    );
}

/// A store that answers `Ok` and keeps nothing, as `LocalProxyStore` does at its signature cap, must
/// not cost the stub: the stub is the recording, and Mountebank has no cap. Without it, every repeat
/// of every new identity would forward forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_keeps_nothing_on_an_ok_settle_still_gets_the_stub() {
    let (manager, port, upstream) = proxying_through(SettlingStore::new(false), "proxyOnce").await;
    assert_eq!(get(port).await, "up");
    assert_eq!(get(port).await, "up");
    let stubs = manager.get_imposter(port).expect("proxy").get_stubs();
    assert_eq!(stubs.len(), 2, "the recording and the proxy: {stubs:?}");
    assert_eq!(
        upstream.get_recorded_requests().len(),
        1,
        "the stub answers"
    );
}

/// `proxyAlways` appends whatever the store says: it has no exactly-once to protect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_always_appends_even_when_its_settle_fails() {
    let (manager, port, _) = proxying_through(SettlingStore::new(true), "proxyAlways").await;
    assert_eq!(get(port).await, "up");
    let stubs = manager.get_imposter(port).expect("proxy").get_stubs();
    assert_eq!(stubs.len(), 2, "the proxy and what it appended: {stubs:?}");
}
