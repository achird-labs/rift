//! Issue #1134: two log-level reads in `main.rs` discarded a failure and fell back to a level the
//! operator did not choose — an unrecognised `--loglevel` became `info` (so `trace`, a real level,
//! was silently downgraded), and `EnvFilter::try_from_default_env().unwrap_or_else(..)` could not
//! tell `RUST_LOG` *unset* from `RUST_LOG` *set and invalid*.
//!
//! The rules now live in `bootstrap::log_filter`. The matrix is unit-tested there, against a
//! supplied value, so it needs no environment mutation. What is left for this file is the part only
//! the real binary can show: that a refusal actually aborts startup, and that `RUST_LOG` is read
//! from the process environment at all. `Command::env` sets the variable in the CHILD only, so
//! these cannot leak into any other test.

mod support;

use std::process::{Command, Output};

/// Run to a point PAST the filter without ever starting a server: `stop` is dispatched after the
/// subscriber is installed, so it reaches `log_filter` and then fails on its missing pidfile.
///
/// **Every case here uses this, including the ones expecting a refusal.** Spawning a bare server and
/// relying on it *not* reaching the accept loop would mean that if a refusal ever regressed, the
/// process would bind and serve forever, `output()` would never return, and the Rust harness — which
/// has no per-test timeout — would hang CI to the job limit instead of reporting a failure. A
/// regression test must fail, not hang.
///
/// `RUST_LOG` is removed unless a case sets it: the filter reads the environment, so a developer who
/// exports an invalid one would otherwise see unrelated cases abort.
fn run_past_the_filter(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(support::server_bin());
    cmd.env_remove("RUST_LOG")
        .env_remove("MB_LOGLEVEL")
        .args(args)
        .args(["--pidfile", "/nonexistent/rift-1134/rift.pid", "stop"]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run rift")
}

/// The marker proving a run got past the filter and reached the `stop` dispatch.
const PAST_THE_FILTER: &str = "PID file not found";

// The headline: a level that does not exist must stop the server rather than quietly becoming
// `info`. The same judgement #1114 applies to a wrong-*typed* rcfile value, for a wrong-*valued* one.
#[test]
fn an_unrecognised_loglevel_aborts_startup_naming_the_value() {
    let out = run_past_the_filter(&["--loglevel", "warnn"], &[]);

    assert!(
        !out.status.success(),
        "an unrecognised --loglevel must abort, not start at info"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warnn"),
        "the refusal must name the offending value, got: {stderr}"
    );
    assert!(
        stderr.contains("trace"),
        "the refusal must list the accepted levels, got: {stderr}"
    );
    assert!(
        !stderr.contains(PAST_THE_FILTER),
        "the refusal must happen at the filter, before the `stop` dispatch: {stderr}"
    );
}

// A `RUST_LOG` that is set and invalid must be refused rather than silently replaced by the CLI
// level. `foo=bar` is invalid because `bar` is not a level — note a bare word like `inf` is a valid
// *target* directive, so it is deliberately not used here (see the unit tests).
#[test]
fn an_invalid_rust_log_aborts_startup() {
    let out = run_past_the_filter(&[], &[("RUST_LOG", "foo=bar")]);

    assert!(
        !out.status.success(),
        "a set-but-invalid RUST_LOG must abort, not fall back to --loglevel"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("RUST_LOG"),
        "the refusal must name the variable, got: {stderr}"
    );
    assert!(
        stderr.contains("foo=bar"),
        "the refusal must name the offending value, got: {stderr}"
    );
    assert!(
        !stderr.contains(PAST_THE_FILTER),
        "the refusal must happen at the filter, before the `stop` dispatch: {stderr}"
    );
}

// An env var set to the empty string is how `MB_LOGLEVEL=${LOG_LEVEL}` arrives when `LOG_LEVEL` is
// unset, and clap prefers an env value over the default whenever the variable is present at all. It
// must keep meaning "not supplied" rather than aborting a deployment that works today.
#[test]
fn an_empty_loglevel_env_still_starts() {
    let out = run_past_the_filter(&[], &[("MB_LOGLEVEL", "")]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("is not a log level"),
        "an empty MB_LOGLEVEL must mean 'not supplied', not a refusal: {stderr}"
    );
    assert!(
        stderr.contains(PAST_THE_FILTER),
        "the run must have reached the `stop` dispatch past the filter, got: {stderr}"
    );
}

// The other side of the same read: a VALID `RUST_LOG` must not be refused. Without this, the
// refusal above is satisfied by a filter that rejects everything.
#[test]
fn a_valid_rust_log_is_not_refused() {
    let out = run_past_the_filter(&[], &[("RUST_LOG", "warn,hyper=off")]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("RUST_LOG"),
        "a valid RUST_LOG must not be refused, got: {stderr}"
    );
    assert!(
        stderr.contains("PID file not found"),
        "the run must have reached the `stop` dispatch past the filter, got: {stderr}"
    );
}

// `trace` is the level the old catch-all silently swallowed, and the reason this is a bug rather
// than a nicety. It must be accepted rather than refused OR downgraded.
#[test]
fn trace_is_accepted_by_the_binary() {
    let out = run_past_the_filter(&["--loglevel", "trace"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("is not a log level"),
        "--loglevel trace must be accepted, got: {stderr}"
    );
    assert!(
        stderr.contains("PID file not found"),
        "the run must have reached the `stop` dispatch past the filter, got: {stderr}"
    );
}
