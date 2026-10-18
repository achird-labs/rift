//! Issue #1559: two things a Mountebank command line did that Rift refused.
//!
//! - `mb stop` with no PID file exits 0 (`bin/mb` resolves `true` when the file does not exist);
//!   `rift stop` exited 1 with `PID file not found`. mbTest calls `stop` in every `afterEach`, so
//!   that exit code aborted whole CLI test files.
//! - Mountebank spells three server flags in camelCase or differently — `--logfile`, `--localOnly`,
//!   `--ipWhitelist` — and Rift answered each with clap's exit 2. mbTest passes `--logfile` to every
//!   `start`.
//!
//! The `stop` cases run the real binary, because the exit code is decided in `main.rs`.

mod support;

use clap::Parser;
use rift_http_proxy::server::Cli;
use std::process::{Command, Output};

/// `try_parse_from`, not `parse_from`: a refused flag must fail the one test, not exit the harness.
fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"))
}

fn rift(args: &[&str]) -> Output {
    Command::new(support::server_bin())
        .env_remove("RUST_LOG")
        .env_remove("MB_LOGLEVEL")
        .args(args)
        .output()
        .expect("run rift")
}

#[test]
fn stop_with_no_pid_file_exits_zero_and_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("missing.pid");
    let out = rift(&["--pidfile", pidfile.to_str().expect("utf-8"), "stop"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`rift stop` with nothing to stop must succeed, as `mb stop` does; got {:?}: {stderr}",
        out.status
    );
    assert!(
        stdout.contains("nothing to stop"),
        "the no-op must be reported, not silent: {stdout}"
    );
    assert!(!pidfile.exists(), "nothing is created by the no-op path");
}

// The half of #827/#1155 that matters stays: a PID file naming no real process is still refused,
// and kept, because it was never acted on.
#[test]
fn stop_with_a_non_positive_pid_still_fails_and_keeps_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("zero.pid");
    std::fs::write(&pidfile, "0").expect("write pidfile");
    let out = rift(&["--pidfile", pidfile.to_str().expect("utf-8"), "stop"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "got: {stderr}");
    assert!(
        stderr.contains("non-positive pid"),
        "the refusal must say why: {stderr}"
    );
    assert!(pidfile.exists(), "a PID file never acted on must be kept");
}

// `--logfile` is an alias of `--log`, not just accepted: the file it names receives the log. The
// `stop` dispatch logs after the file layer is installed, so its line lands there.
#[test]
fn logfile_writes_the_log_to_the_named_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let logfile = dir.path().join("mb.log");
    let pidfile = dir.path().join("missing.pid");
    let out = rift(&[
        "--logfile",
        logfile.to_str().expect("utf-8"),
        "--pidfile",
        pidfile.to_str().expect("utf-8"),
        "stop",
    ]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "`--logfile` must parse: {stderr}");
    let logged = std::fs::read_to_string(&logfile).expect("--logfile must create the file");
    assert!(
        logged.contains("nothing to stop"),
        "the log must reach the --logfile file: {logged}"
    );
}

#[test]
fn logfile_is_an_alias_of_log() {
    let cli = parse(&["rift", "--logfile", "mb.log"]);
    assert_eq!(cli.log.as_deref(), Some(std::path::Path::new("mb.log")));
}

#[test]
fn local_only_camel_case_is_an_alias() {
    assert!(parse(&["rift", "--localOnly"]).local_only);
}

// Still the #879 no-op: the alias binds the same field, which nothing enforces.
#[test]
fn ip_whitelist_camel_case_is_an_alias() {
    let cli = parse(&["rift", "--ipWhitelist", "10.0.0.1,192.168.0.1"]);
    assert_eq!(
        cli.ip_whitelist.as_deref(),
        Some(&["10.0.0.1".to_string(), "192.168.0.1".to_string()][..])
    );
    // Mountebank's own delimiter is `|`. It is accepted too; the value is read nowhere.
    let piped = parse(&["rift", "--ipWhitelist", "127.0.0.1|::1"]);
    assert!(piped.ip_whitelist.is_some());
}

// The aliases are global like their long flags, so they bind after the subcommand too (#1316).
#[test]
fn the_aliases_bind_after_a_subcommand() {
    let cli = parse(&["rift", "start", "--logfile", "f", "--localOnly"]);
    assert_eq!(cli.log.as_deref(), Some(std::path::Path::new("f")));
    assert!(cli.local_only);
}

// `mb save --removeProxies` (mbTest `cli/saveTest.js`). Found once `stop` stopped aborting the file.
#[test]
fn remove_proxies_camel_case_is_an_alias_on_save() {
    let cli = parse(&["rift", "save", "--removeProxies"]);
    match cli.command {
        Some(rift_http_proxy::server::Commands::Save { remove_proxies, .. }) => {
            assert!(remove_proxies)
        }
        other => panic!("expected save, got {other:?}"),
    }
}
