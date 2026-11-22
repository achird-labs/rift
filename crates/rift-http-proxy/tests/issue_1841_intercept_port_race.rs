//! Issue #1841: an imposter created while the intercept listener starts on the same port. Before
//! the fix both reported success: the intercept checked for an imposter, bound, and reserved the
//! port only at install, while the imposter checked the reservations, bound, and claimed after.
//! On macOS the two binds coexist and split the port's clients. Now exactly one of them gets it,
//! on every platform: afterwards the port is either reserved by the intercept or held by a bound
//! imposter, never both.

use rift_http_proxy::imposter::{ImposterConfig, ImposterManager, Point, Probe, ServerListener};
use rift_http_proxy::intercept_control::{
    InterceptControl, InterceptStartError, InterceptStartOptions,
};
use std::sync::{Arc, Condvar};
use std::time::Duration;

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

type Matcher = Box<dyn Fn(&Point) -> bool + Send + Sync>;

#[derive(Default)]
struct Recorder {
    points: parking_lot::Mutex<Vec<Point>>,
    hold: parking_lot::Mutex<Option<(Matcher, Arc<Latch>)>>,
}

impl Probe for Recorder {
    fn at(&self, point: Point) {
        self.points.lock().push(point);
        let held = {
            let mut hold = self.hold.lock();
            match hold.as_ref() {
                Some((matches, _)) if matches(&point) => hold.take().map(|(_, latch)| latch),
                _ => None,
            }
        };
        if let Some(latch) = held {
            tokio::task::block_in_place(|| latch.wait());
        }
    }
}

impl Recorder {
    fn hold(&self, matches: impl Fn(&Point) -> bool + Send + Sync + 'static) -> Arc<Latch> {
        let latch = Arc::new(Latch::default());
        *self.hold.lock() = Some((Box::new(matches), Arc::clone(&latch)));
        latch
    }

    async fn reached(&self, matches: impl Fn(&Point) -> bool) {
        for _ in 0..2000 {
            if self.points.lock().iter().any(&matches) {
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn config(port: u16) -> ImposterConfig {
    serde_json::from_value(serde_json::json!({"port": port, "protocol": "http"}))
        .expect("imposter config")
}

struct Race {
    manager: Arc<ImposterManager>,
    control: InterceptControl,
    port: u16,
    started: Result<(), InterceptStartError>,
    created: Result<u16, rift_http_proxy::imposter::ImposterError>,
}

/// Hold an explicit-port create at `at`, start the intercept listener on the port meanwhile, then
/// let the create finish.
async fn race(at: fn(&Point, u16) -> bool) -> Race {
    let recorder = Arc::new(Recorder::default());
    let manager =
        Arc::new(ImposterManager::new().with_probe(Arc::clone(&recorder) as Arc<dyn Probe>));
    let port = free_port();
    let latch = recorder.hold(move |p| at(p, port));
    let creating = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.create_imposter(config(port)).await })
    };
    recorder.reached(move |p| at(p, port)).await;
    let control = InterceptControl::default().with_port_reservations(Arc::clone(&manager));
    let started = control
        .start(InterceptStartOptions {
            port: Some(port),
            ..Default::default()
        })
        .await
        .map(|_| ());
    latch.open();
    let created = creating.await.expect("create task");
    Race {
        manager,
        control,
        port,
        started,
        created,
    }
}

impl Race {
    /// Exactly one door won, and the port's state says which.
    fn assert_one_owner(&self) {
        assert!(
            self.started.is_ok() != self.created.is_ok(),
            "exactly one of the intercept start ({:?}) and the create ({:?}) may win port {}",
            self.started,
            self.created,
            self.port
        );
        let reserved = self.manager.reserved_by(self.port) == Some(ServerListener::Intercept);
        let bound = self.manager.imposter_listens_on(self.port);
        assert!(
            reserved != bound,
            "the port is reserved by the intercept ({reserved}) or held by a bound imposter \
             ({bound}), never both or neither"
        );
        assert_eq!(self.started.is_ok(), reserved);
        assert_eq!(self.started.is_ok(), self.control.status().is_some());
    }

    async fn stop(self) {
        self.control.stop().await;
        self.manager.delete_all().await;
    }
}

/// The intercept starts while the imposter is bound but not yet claimed. On macOS the intercept's
/// bind succeeds and its reservation refuses the create's claim; on Linux the bind itself fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_intercept_started_while_an_imposter_binds_leaves_one_owner() {
    let race = race(|p, port| matches!(p, Point::CreateBound { port: q, .. } if *q == port)).await;
    race.assert_one_owner();
    race.stop().await;
}

/// The imposter claimed, and is not yet admitting connections, when the intercept starts: the
/// start is refused and the slot left empty, with the refusal the check before the bind gives.
/// (The narrower window, an imposter claiming between that check and the install, is driven
/// directly by `install_refuses_a_plane_whose_port_an_imposter_claimed_during_the_bind`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_intercept_started_after_an_imposter_claimed_is_refused() {
    let race =
        race(|p, port| matches!(p, Point::CreateClaimed { port: q, .. } if *q == port)).await;
    race.assert_one_owner();
    match &race.started {
        Err(InterceptStartError::Bind(e)) => assert!(
            format!("{e:#}").contains("in use"),
            "the refusal says the port is in use: {e:#}"
        ),
        other => panic!("the intercept must be refused with Bind, got {other:?}"),
    }
    assert!(race.created.is_ok());
    race.stop().await;
}
