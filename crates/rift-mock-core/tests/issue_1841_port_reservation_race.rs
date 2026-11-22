//! Issue #1841: a server listener reserving a port and an imposter claiming it are one step
//! against each other, so whichever comes second sees the other and exactly one gets the port.
//!
//! Before the fix both checked the other, bound, and only then published their own claim, so a
//! reservation made while an imposter was binding (or an imposter bound while a listener was
//! starting) left both holding the port: on macOS the two binds coexist and split the clients.
//! Each test holds a create at a lifecycle point with a probe and reserves the port there.

use rift_mock_core::imposter::{
    ImposterConfig, ImposterError, ImposterManager, Point, PortReservation, PortTaken, Probe,
    ServerListener,
};
use std::sync::{Arc, Condvar};
use std::time::Duration;

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

type Matcher = Box<dyn Fn(&Point) -> bool + Send + Sync>;

/// Records every point and holds the engine at the first one a test asks for.
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

    fn saw(&self, matches: impl Fn(&Point) -> bool) -> bool {
        self.points.lock().iter().any(matches)
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

fn manager(recorder: &Arc<Recorder>) -> Arc<ImposterManager> {
    Arc::new(ImposterManager::new().with_probe(Arc::clone(recorder) as Arc<dyn Probe>))
}

fn addr(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// Nothing accepts on `port` within a bounded wait: a refused create's listener is gone.
async fn nothing_accepts_on(port: u16) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr(port)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("a listener still accepts on port {port}");
}

/// The reservation comes while the create is bound but has not claimed: the create is refused
/// when it claims. Before the fix the create claimed anyway and both held the port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_made_while_an_imposter_binds_refuses_its_claim() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = free_port();
    let latch =
        recorder.hold(move |p| matches!(p, Point::CreateBound { port: q, .. } if *q == port));
    let creating = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.create_imposter(config(port)).await })
    };
    recorder
        .reached(|p| matches!(p, Point::CreateBound { .. }))
        .await;

    let reservation = PortReservation::try_new(&manager, &[addr(port)], ServerListener::Intercept)
        .expect("no imposter has claimed the port yet");
    latch.open();
    let created = creating.await.expect("create task");

    assert!(
        matches!(created, Err(ImposterError::PortInUse(p)) if p == port),
        "the create must lose to the reservation: {created:?}"
    );
    assert!(recorder.saw(|p| matches!(p, Point::CreateReservedLost { port: q, .. } if *q == port)));
    assert!(!recorder.saw(|p| matches!(p, Point::CreateClaimed { .. })));
    assert!(
        manager.get_imposter(port).is_err(),
        "nothing was registered"
    );
    assert_eq!(manager.reserved_by(port), Some(ServerListener::Intercept));
    nothing_accepts_on(port).await;
    drop(reservation);
    assert_eq!(manager.reserved_by(port), None);
}

/// The imposter claimed before the reservation: the reservation is refused and leaves nothing
/// reserved. Before the fix `PortReservation::new` reserved a port an imposter accepted on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_after_the_claim_is_refused() {
    let recorder = Arc::new(Recorder::default());
    let manager = manager(&recorder);
    let port = free_port();
    let latch =
        recorder.hold(move |p| matches!(p, Point::CreateClaimed { port: q, .. } if *q == port));
    let creating = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.create_imposter(config(port)).await })
    };
    recorder
        .reached(|p| matches!(p, Point::CreateClaimed { .. }))
        .await;

    let refused = PortReservation::try_new(&manager, &[addr(port)], ServerListener::Intercept);
    assert_eq!(
        refused.as_ref().err(),
        Some(&PortTaken {
            port,
            by: ServerListener::Intercept
        })
    );
    assert_eq!(
        manager.reserved_by(port),
        None,
        "a refusal reserves nothing"
    );
    let message = std::io::Error::from(refused.expect_err("refused")).to_string();
    assert!(
        message.contains("already in use by an imposter") && message.contains("intercept proxy"),
        "the refusal reads like the check before the bind: {message}"
    );

    latch.open();
    let created = creating.await.expect("create task");
    assert!(matches!(created, Ok(p) if p == port), "{created:?}");
    assert!(manager.imposter_listens_on(port));
    manager.delete_all().await;
}

/// A serve-unbound apply that loses the port to a reservation registers the imposter without a
/// listener, as it does when the port was reserved before its bind, so in-process routes still
/// serve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_serve_unbound_create_that_loses_the_port_registers_unbound() {
    let recorder = Arc::new(Recorder::default());
    let manager = Arc::new(
        ImposterManager::new()
            .with_serve_unbound(true)
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let port = free_port();
    let latch =
        recorder.hold(move |p| matches!(p, Point::CreateBound { port: q, .. } if *q == port));
    let applying = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.apply_config(vec![config(port)]).await })
    };
    recorder
        .reached(|p| matches!(p, Point::CreateBound { .. }))
        .await;
    let _reservation = PortReservation::try_new(&manager, &[addr(port)], ServerListener::FrontDoor)
        .expect("no imposter has claimed the port yet");
    latch.open();
    let applied = applying.await.expect("apply task");
    assert!(applied.is_ok(), "serve-unbound registers it: {applied:?}");

    let imposter = manager.get_imposter(port).expect("registered");
    assert!(!imposter.is_bound(), "registered without a listener");
    assert!(!manager.imposter_listens_on(port));
    nothing_accepts_on(port).await;
    manager.delete_all().await;
}

/// All or nothing: one port an imposter holds refuses the whole set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_reservation_reserves_none_of_its_ports() {
    let manager = Arc::new(ImposterManager::new());
    let held = free_port();
    manager.create_imposter(config(held)).await.expect("create");
    let free = free_port();
    let refused = manager.try_reserve_ports(&[free, held], ServerListener::Admin);
    assert_eq!(
        refused,
        Err(PortTaken {
            port: held,
            by: ServerListener::Admin
        })
    );
    assert_eq!(manager.reserved_by(free), None);
    assert_eq!(manager.reserved_by(held), None);
    assert_eq!(
        manager.try_reserve_ports(&[free], ServerListener::Admin),
        Ok(())
    );
    assert_eq!(manager.reserved_by(free), Some(ServerListener::Admin));
    manager.delete_all().await;
}
