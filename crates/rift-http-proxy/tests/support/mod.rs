//! Shared support for the process-spawning integration suites (`mountebank_compatibility`,
//! `rift_extensions`, `issue_360_script_cli`, `corpus_replay`).
//!
//! This lives at `tests/support/mod.rs`, not `tests/support.rs`: Cargo treats every `.rs` file
//! directly under `tests/` as its own test binary target, so a bare `tests/support.rs` would be
//! compiled and run as an (empty) test suite in its own right. The `support/mod.rs` layout is a
//! module, not a target, and each suite pulls it in with `#[path = "support/mod.rs"] mod support;`
//! (or an equivalent `mod support;` alongside a `tests/support/mod.rs` file — either resolves the
//! same way).

use std::path::PathBuf;

/// Path to the rift **server** binary under test (the `rift-http-proxy` process these suites spawn
/// and drive over the admin API / CLI).
///
/// Resolution order:
/// 1. `RIFT_SERVER_BIN`, if set — must name an existing, executable file. This lets any
///    Mountebank-compatible build (a differently-packaged rift, e.g. a musl static image, or a
///    downstream binary that composes rift's `ServerBuilder`) be driven through these suites
///    instead of this repo's own debug build. A set-but-bad override panics rather than falling
///    back silently: quietly testing the wrong binary is worse than failing loudly.
/// 2. Otherwise `CARGO_BIN_EXE_rift-http-proxy` — today's behaviour, unchanged.
///
/// Only the **server** is overridable this way. `corpus_replay.rs` also spawns `rift-verify`
/// (the reference `_verify` replayer); that stays pinned to `CARGO_BIN_EXE_rift-verify` — it is
/// part of the test harness, not the binary under test, and `RIFT_SERVER_BIN` deliberately does
/// not touch it. (Hence `RIFT_SERVER_BIN`, not a generic `RIFT_BIN`: with two binaries in play,
/// a generic name would be ambiguous about which one it overrides.)
pub fn server_bin() -> PathBuf {
    if let Ok(path) = std::env::var("RIFT_SERVER_BIN") {
        let bin = PathBuf::from(&path);
        let executable = bin
            .metadata()
            .map(|m| m.is_file() && is_executable(&m))
            .unwrap_or(false);
        assert!(
            executable,
            "RIFT_SERVER_BIN={path} does not name an existing, executable file"
        );
        return bin;
    }
    PathBuf::from(env!("CARGO_BIN_EXE_rift-http-proxy"))
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    // No portable executable-bit check off Unix; existence-as-a-file is the best we can assert.
    true
}

/// Spawn the server with `--port 0` plus `args`, and return it with the admin port it bound.
///
/// The port comes from the server's own `listening on http://<addr>` startup line, so no probe
/// socket is ever opened and closed (#1474: a probed port can be taken, or its closing probe can
/// reset a neighbour's connection, because the server binds `0.0.0.0` while a probe holds
/// `127.0.0.1`). Colour is disabled with `NO_COLOR`, and ANSI sequences are stripped anyway in case
/// the override is not honoured. Stdout and stderr keep being drained after the port is read so the
/// child can never block on a full pipe. Panics with everything the child printed if it exits or
/// stays silent past the 10 s deadline.
#[allow(dead_code)] // each suite compiles this module; not all of them spawn through it
pub async fn spawn_server(args: &[&str]) -> (tokio::process::Child, u16) {
    spawn_server_with_env(args, &[]).await
}

/// [`spawn_server`] with `envs` set in the child's environment.
#[allow(dead_code)] // each suite compiles this module; not all of them spawn through it
pub async fn spawn_server_with_env(
    args: &[&str],
    envs: &[(&str, &str)],
) -> (tokio::process::Child, u16) {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

    fn forward(
        stream: impl AsyncRead + Unpin + Send + 'static,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // The receiver is gone once the port is found; keep reading to drain the pipe.
                let _ = tx.send(line);
            }
        });
    }

    let mut child = tokio::process::Command::new(server_bin())
        .args(["--port", "0"])
        .args(args)
        .env("NO_COLOR", "1")
        .envs(envs.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rift server");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    forward(child.stdout.take().expect("piped stdout"), tx.clone());
    forward(child.stderr.take().expect("piped stderr"), tx);

    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = rx.recv().await {
            let plain = strip_ansi(&line);
            if let Some(port) = admin_port_from(&plain) {
                return Some(port);
            }
            seen.push(plain);
        }
        None // both pipes closed: the child exited
    })
    .await;
    match found {
        Ok(Some(port)) => (child, port),
        Ok(None) => panic!(
            "rift server exited before reporting its admin port; output:\n{}",
            seen.join("\n")
        ),
        Err(_) => panic!(
            "rift server did not report its admin port within 10s; output:\n{}",
            seen.join("\n")
        ),
    }
}

/// The port in `... listening on http://<host>:<port>`, if `line` is the admin listener's line.
fn admin_port_from(line: &str) -> Option<u16> {
    let rest = line.split("listening on http://").nth(1)?;
    let addr = rest.split_whitespace().next()?;
    addr.rsplit(':').next()?.parse().ok()
}

/// Drop `ESC [ ... <final byte>` colour sequences.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
