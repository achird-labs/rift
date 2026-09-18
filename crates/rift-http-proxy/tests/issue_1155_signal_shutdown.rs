//! Issue #1155: the `rift` binary installed no signal handler, so `SIGTERM`/`SIGINT` took their
//! default disposition. Outside a container the process died instantly with no cleanup; as a
//! container's PID 1, where the kernel drops a signal that has no handler, `docker stop` waited out
//! its whole timeout and then `SIGKILL`ed. The CLI docs have always promised "graceful shutdown".
//!
//! These run the real binary, because signal disposition is a property of the process.
//!
//! The datadir assertion is the one that matters most. The issue proposed driving "the existing
//! embedder shutdown path" — but the FFI's `rift_stop` calls `ImposterManager::shutdown`, which
//! deletes every imposter *and unlinks its persisted file*. Wired to SIGTERM, that would make every
//! `docker stop` wipe the datadir. A graceful stop must leave persisted state exactly where it was.
#![cfg(unix)]

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The shutdown is bounded at roughly 3s (≤500ms accept + ≤500ms connections, per plane); anything
/// past this is a hang, not a slow drain.
const EXIT_BUDGET: Duration = Duration::from_secs(5);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Server {
    child: Child,
    admin_port: u16,
    pidfile: PathBuf,
    datadir: PathBuf,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

fn spawn_server(extra: &[&str]) -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("rift.pid");
    let datadir = dir.path().join("data");
    std::fs::create_dir_all(&datadir).expect("datadir");
    let log = dir.path().join("rift.log");
    let admin_port = free_port();
    let admin_s = admin_port.to_string();
    let mut args = vec![
        "--port",
        &admin_s,
        "--host",
        "127.0.0.1",
        "--pidfile",
        pidfile.to_str().expect("utf-8"),
        "--datadir",
        datadir.to_str().expect("utf-8"),
        "--log",
        log.to_str().expect("utf-8"),
        "--metrics-port",
        "0",
    ];
    args.extend_from_slice(extra);
    let mut command = Command::new(support::server_bin());
    command
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A signal mask survives exec, and a test harness can run with SIGINT blocked — then the kernel
    // holds SIGINT pending forever and no handler can run, which would test the harness, not the
    // server. Docker, systemd, Kubernetes and an interactive shell do not block it, so start each
    // server with a clean mask: what is under test is the handler.
    // SAFETY: `pre_exec` runs in the forked child before exec; sigemptyset/sigprocmask are
    // async-signal-safe and touch only the child's own mask.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            if libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn rift");
    let mut server = Server {
        child,
        admin_port,
        pidfile,
        datadir,
        log,
        _dir: dir,
    };
    wait_ready(&mut server);
    server
}

fn wait_ready(server: &mut Server) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        // A server that died at startup (e.g. a parallel test took the port first) fails here, with
        // its exit status, rather than as a 20s timeout.
        if let Some(status) = server.child.try_wait().expect("try_wait") {
            let log = std::fs::read_to_string(&server.log).unwrap_or_default();
            panic!("rift exited during startup with {status:?}: {log}");
        }
        if std::net::TcpStream::connect(("127.0.0.1", server.admin_port)).is_ok()
            && server.pidfile.exists()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("rift did not come up on {}", server.admin_port);
}

/// Create an imposter over the admin API, so the datadir holds persisted state a stop could lose.
fn create_imposter(server: &Server) -> (u16, PathBuf) {
    let imposter_port = free_port();
    let body = format!(r#"{{"port":{imposter_port},"protocol":"http","stubs":[]}}"#);
    let status = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "--data",
            &body,
            &format!("http://127.0.0.1:{}/imposters", server.admin_port),
        ])
        .output()
        .expect("curl");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "201",
        "imposter creation must succeed"
    );
    let persisted = server.datadir.join(format!("{imposter_port}.json"));
    assert!(
        persisted.exists(),
        "the imposter must be persisted before the stop"
    );
    (imposter_port, persisted)
}

fn signal(child: &Child, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
    // SAFETY: sending a signal to a child this test spawned and still owns.
    let rc = unsafe { libc::kill(pid, sig) };
    assert_eq!(rc, 0, "kill({pid}, {sig}) failed");
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + EXIT_BUDGET;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("rift did not exit within {EXIT_BUDGET:?} of the signal");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_graceful(mut server: Server, sig: libc::c_int) {
    let (_, persisted) = create_imposter(&server);
    signal(&server.child, sig);
    let status = wait_exit(&mut server.child);

    assert_eq!(
        status.code(),
        Some(0),
        "a graceful stop exits 0 — not 143/130 from the default disposition, got {status:?}"
    );
    assert!(
        !server.pidfile.exists(),
        "the server must remove the PID file it wrote"
    );
    let saved = std::fs::read_to_string(&persisted)
        .expect("the persisted imposter must SURVIVE a graceful stop");
    serde_json::from_str::<serde_json::Value>(&saved).expect("and still parse");
    let log = std::fs::read_to_string(&server.log).unwrap_or_default();
    assert!(
        log.contains("shutting down"),
        "the --log file must hold the shutdown line: {log}"
    );
}

#[test]
fn sigterm_shuts_down_gracefully_and_keeps_the_datadir() {
    assert_graceful(spawn_server(&[]), libc::SIGTERM);
}

#[test]
fn sigint_shuts_down_gracefully_too() {
    assert_graceful(spawn_server(&[]), libc::SIGINT);
}

// The per-core runtime has its own serving arm in `main`. macOS downgrades it to work-stealing, so
// only Linux proves the arm itself.
#[cfg(target_os = "linux")]
#[test]
fn the_per_core_runtime_shuts_down_the_same_way() {
    assert_graceful(spawn_server(&["--runtime", "per-core"]), libc::SIGTERM);
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

// `rift stop` used to send SIGTERM, delete the PID file and return at once — reporting success
// while the server was still running, which made `restart` race its own rebind. It now returns
// only once the process is gone, and tolerates the server having removed the PID file first.
#[test]
fn rift_stop_returns_only_after_the_server_has_exited() {
    let server = spawn_server(&[]);
    let pid = libc::pid_t::try_from(server.child.id()).expect("pid");
    let pidfile = server.pidfile.clone();

    // This test is the server's parent, so an exited server stays a zombie until the test reaps it
    // — and `rift stop` probes with `kill(pid, 0)`, which still reports a zombie as existing. In real
    // use `rift stop` is never the parent. Reap concurrently, so the probe sees what a real one would.
    let Server {
        mut child, _dir, ..
    } = server;
    // The reaper records the moment the server exited; `stop_returned` the moment `rift stop` did.
    // The property is their ORDER. Asserting "the process is gone" after joining the reaper would be
    // vacuous — the join itself waits for the exit.
    let reaper = std::thread::spawn(move || (child.wait(), Instant::now()));

    let out = Command::new(support::server_bin())
        .args(["--pidfile", pidfile.to_str().expect("utf-8"), "stop"])
        .output()
        .expect("run rift stop");
    let stop_returned = Instant::now();
    let (status, server_exited) = reaper.join().expect("reaper");
    let status = status.expect("wait");
    drop(_dir);
    assert!(
        server_exited <= stop_returned,
        "rift stop returned {:?} BEFORE the server exited",
        server_exited.duration_since(stop_returned)
    );

    assert!(
        out.status.success(),
        "rift stop must succeed even though the server removed its PID file first: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !process_exists(pid),
        "rift stop must not return while the server is still running"
    );
    assert_eq!(status.code(), Some(0), "the stopped server exits 0");
}

// A request whose script never finishes must not hold the shutdown hostage (issue #1155 review).
// A `decorate` runs on tokio's `spawn_blocking`, and Boa cannot be interrupted, so the blocking thread
// outlives the request's own timeout. `server.shutdown()` is bounded, but the runtime is dropped
// afterwards, and tokio's `BlockingPool::drop` waits for every blocking task with NO limit — so
// without an explicit bound SIGTERM would hang for ever: worse than the default disposition it
// replaced, which at least ended the process.
//
// It has to be a `decorate`. An `inject` response runs on a dedicated JS worker pool, whose threads
// do not block runtime drop — a test written with `inject` passes against the unfixed code.
#[test]
fn sigterm_exits_even_while_a_script_is_still_running() {
    let mut server = spawn_server(&["--allowInjection"]);
    let imposter_port = free_port();
    // The outer loop never ends; the inner one keeps each outer iteration well under Boa's per-loop
    // iteration cap, so the cap never frees the thread either.
    let body = format!(
        r#"{{"port":{imposter_port},"protocol":"http","stubs":[{{"responses":[{{"is":{{"statusCode":200}},"_behaviors":{{"decorate":"function (request, response) {{ for (;;) {{ for (var i = 0; i < 1000000; i++) {{}} }} }}"}}}}]}}]}}"#
    );
    let created = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "--data",
            &body,
            &format!("http://127.0.0.1:{}/imposters", server.admin_port),
        ])
        .output()
        .expect("curl");
    assert_eq!(String::from_utf8_lossy(&created.stdout), "201");

    // Occupy a blocking thread with the script. The client gives up; the thread does not.
    let url = format!("http://127.0.0.1:{imposter_port}/spin");
    let request = std::thread::spawn(move || {
        let _ = std::process::Command::new("curl")
            .args(["-s", "-m", "8", &url])
            .output();
    });
    std::thread::sleep(Duration::from_millis(800));

    signal(&server.child, libc::SIGTERM);
    let status = wait_exit(&mut server.child);
    assert_eq!(
        status.code(),
        Some(0),
        "a stuck script must not stop a graceful exit, got {status:?}"
    );
    let _ = request.join();
}

// The server removes the PID file only while it still names this process. A second server started
// on the same `--pidfile` has taken the file over; removing it would hide that live server from
// `rift stop`.
#[test]
fn sigterm_leaves_a_pidfile_another_process_has_taken_over() {
    let mut server = spawn_server(&[]);
    std::fs::write(&server.pidfile, "1").expect("take the PID file over");
    signal(&server.child, libc::SIGTERM);
    let status = wait_exit(&mut server.child);
    assert_eq!(status.code(), Some(0), "got {status:?}");
    assert_eq!(
        std::fs::read_to_string(&server.pidfile).ok().as_deref(),
        Some("1"),
        "a PID file naming another process must be left alone"
    );
}
