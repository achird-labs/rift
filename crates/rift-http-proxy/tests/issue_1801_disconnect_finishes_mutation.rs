//! Issue #1801: once an admin request's body has been read, the mutation finishes even if the client
//! disconnects, as Mountebank's handlers do. hyper drops a connection's service future when the
//! peer goes away (HTTP/1 EOF mid-message, HTTP/2 `RST_STREAM`), so a handler run on the connection
//! stopped at its next `.await`: a `PUT /imposters` swept half its set, and a delete left its
//! imposter registered with its file unlinked.
//!
//! Each test holds the engine at a lifecycle point with a probe, closes the client socket while it
//! is held (the body is read by then), then lets it go, and checks the server finished the work nobody is waiting for.

use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::imposter::{ImposterManager, Point, Probe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
}

struct Fixture {
    recorder: Arc<Recorder>,
    manager: Arc<ImposterManager>,
    datadir: PathBuf,
    admin: RunningAdminApi,
}

async fn fixture() -> Fixture {
    let recorder = Arc::new(Recorder::default());
    // Kept for the test's lifetime: the directory is deleted with the handle.
    let datadir = tempfile::tempdir().expect("datadir").keep();
    let manager = Arc::new(
        ImposterManager::with_datadir(Some(datadir.clone()))
            .with_probe(Arc::clone(&recorder) as Arc<dyn Probe>),
    );
    let admin = AdminApiServer::new(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::clone(&manager),
        None,
    )
    .bind()
    .await
    .expect("bind admin API");
    Fixture {
        recorder,
        manager,
        datadir,
        admin,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn imposter_json(port: u16) -> serde_json::Value {
    serde_json::json!({
        "port": port,
        "protocol": "http",
        "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]
    })
}

/// Send a whole request and return the socket unread; dropping it hangs up.
async fn send(admin: &RunningAdminApi, method: &str, path: &str, body: &str) -> TcpStream {
    let mut stream = TcpStream::connect(admin.local_addr())
        .await
        .expect("connect");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.flush().await.expect("flush");
    stream
}

/// Wait until `done` holds, or give up after a bound and report what it saw.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

fn file(datadir: &Path, port: u16) -> PathBuf {
    datadir.join(format!("{port}.json"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_put_whose_client_hangs_up_still_applies_the_whole_set() {
    let f = fixture().await;
    let ports = [free_port(), free_port(), free_port()];
    let body = serde_json::json!({
        "imposters": ports.iter().map(|p| imposter_json(*p)).collect::<Vec<_>>()
    })
    .to_string();

    let second = ports[1];
    let held = f
        .recorder
        .hold(move |p| matches!(p, Point::CreateClaimed { port, .. } if *port == second));
    let client = send(&f.admin, "PUT", "/imposters", &body).await;
    f.recorder
        .reached(move |p| matches!(p, Point::CreateClaimed { port, .. } if *port == second))
        .await;
    // The body is read and the handler is inside the manager: hang up now, and give the connection
    // time to see the peer gone before the handler resumes.
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    held.open();

    let manager = Arc::clone(&f.manager);
    let datadir = f.datadir.clone();
    eventually(
        "a disconnect dropped the PUT partway: not every imposter is registered and persisted",
        move || {
            ports
                .iter()
                .all(|p| manager.get_imposter(*p).is_ok() && file(&datadir, *p).exists())
        },
    )
    .await;
    let _ = f.manager.delete_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_whose_client_hangs_up_still_deletes() {
    let f = fixture().await;
    let port = free_port();
    f.manager
        .create_imposter(serde_json::from_value(imposter_json(port)).expect("config"))
        .await
        .expect("create");
    assert!(file(&f.datadir, port).exists());

    let held = f
        .recorder
        .hold(|p| matches!(p, Point::DeleteLookedUp { .. }));
    let client = send(&f.admin, "DELETE", &format!("/imposters/{port}"), "").await;
    f.recorder
        .reached(|p| matches!(p, Point::DeleteLookedUp { .. }))
        .await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    held.open();

    let manager = Arc::clone(&f.manager);
    let datadir = f.datadir.clone();
    eventually(
        "a disconnect dropped the delete partway: the imposter or its file is still there",
        move || manager.get_imposter(port).is_err() && !file(&datadir, port).exists(),
    )
    .await;
}
