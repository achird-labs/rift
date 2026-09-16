//! Issue #1133: `healthcheck` was dispatched *before* `--rcfile` was applied, so a deployment that
//! set the admin port in an rcfile ran a server on that port and a container probe that computed
//! its URL from the unmodified `cli.port`, 2525 — reporting unhealthy forever, with nothing in the
//! output mentioning the rcfile.
//!
//! These run the real binary, because the decision is the dispatch order in `main.rs`, not anything
//! the library can be asked. Same reasoning as `issue_1114_rcfile_refusal.rs`.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

/// How long a *positive* arrival assertion waits. The listener thread sends only after it has
/// written the response, so although the child process has already exited by the time we look,
/// `try_recv` would give that thread zero margin. A negative assertion needs no timeout: nothing is
/// ever sent on a listener that was never contacted, so its emptiness is not a race.
const ARRIVED: Duration = Duration::from_secs(5);

/// Bind loopback on an ephemeral port and answer the first request with `200 OK`, reporting on the
/// channel that a probe actually arrived. The report is the discriminator: it proves the probe
/// knocked on *this* port rather than on 2525.
fn spawn_healthy_admin() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe target");
    let port = listener.local_addr().expect("local_addr").port();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
            );
            let _ = stream.flush();
            // A send failure means the test already finished; nothing to do about it here.
            let _ = tx.send(());
        }
    });

    (port, rx)
}

fn write_rcfile(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("rift.rc");
    std::fs::write(&path, body).expect("write rcfile");
    path
}

fn healthcheck(args: &[&str]) -> Output {
    Command::new(support::server_bin())
        .args(args)
        .output()
        .expect("run healthcheck")
}

// The issue's own pin: an rcfile-set port must be the port the probe knocks on.
#[test]
fn healthcheck_probes_the_port_the_rcfile_sets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (port, probed) = spawn_healthy_admin();
    let rcfile = write_rcfile(dir.path(), &format!(r#"{{"port": {port}}}"#));

    let out = healthcheck(&["--rcfile", rcfile.to_str().expect("utf-8"), "healthcheck"]);

    assert!(
        out.status.success(),
        "an rcfile-set port must be probed, not 2525. stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    probed
        .recv_timeout(ARRIVED)
        .expect("the probe must have connected to the rcfile's port");
}

// `host` comes from the same file and must travel with the port.
//
// Proving that takes care. `--host` defaults to `0.0.0.0` and `default_url` maps every bind-any
// spelling to `127.0.0.1`, so an rcfile saying `"host": "127.0.0.1"` produces the *same* URL as an
// rcfile saying nothing at all — a test written that way passes with the `host` key deleted from
// `apply_rcfile_defaults_reporting` entirely, and only re-tests the port.
//
// So the file names an address the probe must NOT be able to reach (TEST-NET-1, RFC 5737,
// guaranteed unroutable) while the listener sits on exactly the address the default would have
// mapped to. If `host` is applied the probe fails and the listener is never touched; if `host` were
// ignored the probe would find the listener and succeed. The failure IS the evidence.
#[test]
fn healthcheck_probes_the_host_the_rcfile_sets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (port, probed) = spawn_healthy_admin(); // on 127.0.0.1 — where the default host maps
    let rcfile = write_rcfile(
        dir.path(),
        &format!(r#"{{"host": "192.0.2.1", "port": {port}}}"#),
    );

    let out = healthcheck(&[
        "--rcfile",
        rcfile.to_str().expect("utf-8"),
        "healthcheck",
        "--timeout",
        "1",
    ]);

    assert!(
        !out.status.success(),
        "the rcfile's host must be probed; reaching the loopback listener means it was ignored"
    );
    assert!(
        probed.try_recv().is_err(),
        "the loopback listener must not be contacted when the rcfile names another host"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("192.0.2.1"),
        "the failure must name the host actually probed, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// An explicit flag still outranks the file here, exactly as it does for a server start — the probe
// must not start preferring the file just because it now reads it.
#[test]
fn an_explicit_port_flag_still_outranks_the_rcfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (flag_port, flag_probed) = spawn_healthy_admin();
    let (file_port, file_probed) = spawn_healthy_admin();
    let rcfile = write_rcfile(dir.path(), &format!(r#"{{"port": {file_port}}}"#));

    let out = healthcheck(&[
        "--rcfile",
        rcfile.to_str().expect("utf-8"),
        "--port",
        &flag_port.to_string(),
        "healthcheck",
    ]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    flag_probed
        .recv_timeout(ARRIVED)
        .expect("the explicit --port must be the one probed");
    assert!(
        file_probed.try_recv().is_err(),
        "the rcfile's port must not be probed when --port was given"
    );
}

// Consistency with issue #1114: a server started with this file would not start, so "unhealthy" is
// the true answer for a probe that reads it. Before this change the file was never read at all, so
// the probe cheerfully reported on 2525.
#[test]
fn a_refused_rcfile_refuses_the_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcfile = write_rcfile(dir.path(), r#"{"port": "not-a-port"}"#);

    let out = healthcheck(&["--rcfile", rcfile.to_str().expect("utf-8"), "healthcheck"]);

    assert!(
        !out.status.success(),
        "a refused rcfile must fail the probe"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("port"),
        "the refusal must name the offending key, got: {stderr}"
    );
}

// A missing rcfile is refused for the probe too, and names the path — the failure an operator would
// otherwise chase as "the server is down".
#[test]
fn a_missing_rcfile_refuses_the_probe_naming_the_path() {
    let missing = "/nonexistent/rift-1133/rc.json";
    let out = healthcheck(&["--rcfile", missing, "healthcheck"]);

    assert!(
        !out.status.success(),
        "a missing rcfile must fail the probe"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(missing),
        "the refusal must name the file it could not read"
    );
}

// The path with no rcfile at all must be untouched by the move.
#[test]
fn healthcheck_without_an_rcfile_still_probes_the_explicit_port() {
    let (port, probed) = spawn_healthy_admin();

    let out = healthcheck(&["--port", &port.to_string(), "healthcheck"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    probed
        .recv_timeout(ARRIVED)
        .expect("the probe must have connected");
}

// An explicit `--url` bypasses host/port entirely and must keep doing so.
#[test]
fn an_explicit_url_still_wins_over_the_rcfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url_port, url_probed) = spawn_healthy_admin();
    let (file_port, file_probed) = spawn_healthy_admin();
    let rcfile = write_rcfile(dir.path(), &format!(r#"{{"port": {file_port}}}"#));

    let out = healthcheck(&[
        "--rcfile",
        rcfile.to_str().expect("utf-8"),
        "healthcheck",
        "--url",
        &format!("http://127.0.0.1:{url_port}/health"),
    ]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    url_probed
        .recv_timeout(ARRIVED)
        .expect("--url must be the one probed");
    assert!(
        file_probed.try_recv().is_err(),
        "the rcfile's port must not be probed when --url was given"
    );
}

// `script` stays ahead of the rcfile: it reads no host or port, so it must not be refused by a file
// it has no use for. This is the ordering constraint the fix had to preserve.
//
// The assertion is deliberately about the *rcfile* and not about the script's own verdict: whether
// this particular script passes `check` is `script_cli`'s business, and pinning it here would make
// this test fail for a reason that has nothing to do with dispatch order. What must be true is that
// the unreadable rcfile is never even looked at.
#[test]
fn script_still_runs_ahead_of_a_broken_rcfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("s.js");
    std::fs::write(&script, "function respond(request, state) { return {}; }")
        .expect("write script");
    let missing_rcfile = "/nonexistent/rift-1133/rc.json";

    let out = healthcheck(&[
        "--rcfile",
        missing_rcfile,
        "script",
        "check",
        script.to_str().expect("utf-8"),
    ]);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains(missing_rcfile),
        "`script` must run without reading the rcfile, but the output names it: {combined}"
    );
}
