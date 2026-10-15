//! Issues #1036 and #1438: tests do not write fixed ports.
//!
//! # Why
//!
//! A fixed port is bound twice whenever the same test binary runs twice at once: `ship-issues`
//! worktrees building side by side, `--lib` and `--tests` in two terminals, or two loops of one
//! file. (`cargo test` itself runs binaries one after another; it parallelises `#[test]`
//! functions within one.) Nothing announces the collision. Imposter listeners bind with
//! `SO_REUSEADDR` **and** `SO_REUSEPORT` (`proxy/network.rs`), so both binds succeed and the kernel
//! load-balances: the loser gets the *other* imposter's response and fails on a decode or status
//! mismatch pointing nowhere near the cause (#999, #1397). Unique ports per file (#1036) cannot
//! prevent that, so tests take a port from the manager or the OS instead (CONTRIBUTING, "Fixed
//! test ports"), and this guard refuses any literal in the old fixed-port block.
//!
//! Modelled on `no_blanket_dead_code_allow.rs`, including an anti-vacuity floor and a test pinning
//! the matcher — a guard whose matcher silently stopped matching would pass forever.

use std::path::{Path, PathBuf};

/// The block fixed test ports used to be taken from (#1036), now refused outright (#1438).
const PORT_LO: u16 = 15000;
const PORT_HI: u16 = 24999;

/// Literals in the block that are not ports, with the reason. Keep this list short: a number that is
/// not a port is better written with a separator (`20_000`), which the matcher skips.
const EXCEPTIONS: &[(&str, &str)] = &[(
    "15000",
    "a lint fixture URL (`http://localhost:15000`) in rift-lint's I002 rule and its test — parsed \
     as text, never bound",
)];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ is the parent of this crate")
        .to_path_buf()
}

/// Every `.rs` file under `crates/*/tests/**` and `crates/*/src/**`.
///
/// `src/**` is not optional: lib unit tests bind ports too, and CI runs `--lib` and `--tests` on
/// *different runners*, so a lib-vs-integration collision is invisible there by construction.
fn rust_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                // This file itself. Its literals are the rule's own data — the reserved-range
                // bounds and the matcher's fixtures — not ports anything binds, so scanning it
                // makes the guard report itself. Excluded by name rather than by some heuristic
                // about "looks like a constant", because a heuristic here would eventually excuse
                // a real collision.
                && path.file_name().is_some_and(|n| n != "test_port_uniqueness.rs")
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for krate in std::fs::read_dir(crates_dir())
        .expect("read crates/")
        .flatten()
    {
        for sub in ["tests", "src"] {
            let dir = krate.path().join(sub);
            if dir.is_dir() {
                walk(&dir, &mut out);
            }
        }
    }
    out.sort();
    out
}

/// Port-shaped integer literals in `text`, with 1-based line numbers.
///
/// Deliberately literal-only. It cannot know whether a number is bound, and it does not try: a
/// number in this range that is *not* a port is rare enough to exception-list, whereas a matcher
/// clever enough to tell them apart would be the thing most likely to break silently.
fn port_literals(text: &str) -> Vec<(u16, usize)> {
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let lb = line.as_bytes();
        let mut i = 0;
        while i < lb.len() {
            if !lb[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let start = i;
            while i < lb.len() && lb[i].is_ascii_digit() {
                i += 1;
            }
            // Reject a digit run that is part of a longer word (`x19500`, `19500u16`, `1_9500`):
            // `\b`-equivalent on both sides, so a suffixed or prefixed number is not a port.
            let before_ok =
                start == 0 || !(lb[start - 1].is_ascii_alphanumeric() || lb[start - 1] == b'_');
            let after_ok = i == lb.len() || !(lb[i].is_ascii_alphanumeric() || lb[i] == b'_');
            if before_ok
                && after_ok
                && i - start == 5
                && let Ok(n) = line[start..i].parse::<u16>()
                && (PORT_LO..=PORT_HI).contains(&n)
            {
                out.push((n, lineno + 1));
            }
        }
    }
    out
}

fn rel(path: &Path) -> String {
    path.strip_prefix(crates_dir())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[test]
fn no_test_writes_a_fixed_port() {
    let files = rust_sources();
    // Anti-vacuity: if the walk silently stopped finding files, the assertion below would pass on
    // nothing. Well under the real number so ordinary churn cannot trip it.
    assert!(
        files.len() > 100,
        "expected to scan >100 Rust sources, found {} — the walk is broken, and a guard that \
         scans nothing passes forever",
        files.len()
    );

    let exempt: Vec<u16> = EXCEPTIONS
        .iter()
        .map(|(p, _)| p.parse().expect("exception port parses"))
        .collect();
    let mut found: Vec<String> = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read source");
        for (port, line) in port_literals(&text) {
            if !exempt.contains(&port) {
                found.push(format!("  {}:{line}: {port}", rel(path)));
            }
        }
    }
    assert!(
        found.is_empty(),
        "tests do not write fixed ports: two concurrent runs of one binary bind each twice, and \
         SO_REUSEPORT means neither gets AddrInUse — they serve each other's traffic (#1438). Omit \
         the port and use what `create_imposter` returns, bind `127.0.0.1:0` for a listener the \
         test owns, or use `free_port()` when the port must be written into text first (see \
         CONTRIBUTING, \"Fixed test ports\"). A number here that is not a port: write it as \
         `20_000`.\n{}",
        found.join("\n")
    )
}

/// Pins the matcher itself. Without this, a matcher that quietly stopped recognising ports would
/// leave the guard above green forever — the failure mode the anti-vacuity floors only partly
/// cover, since they would still pass on a matcher that was merely too narrow.
#[test]
fn the_port_matcher_accepts_ports_and_rejects_look_alikes() {
    let found = |s: &str| -> Vec<u16> { port_literals(s).into_iter().map(|(p, _)| p).collect() };

    assert_eq!(found("let port = 19500;"), vec![19500]);
    assert_eq!(found(r#""http://127.0.0.1:21500/x""#), vec![21500]);
    assert_eq!(found("(15000, 24999)"), vec![15000, 24999]);

    assert_eq!(
        found("Duration::from_millis(20000)"),
        vec![20000],
        "a duration that happens to look like a port IS matched — the matcher cannot tell, which \
         is why the exception list exists rather than a cleverer regex"
    );

    assert!(
        found("let x = 14999;").is_empty(),
        "below the sanctioned block"
    );
    assert!(
        found("let x = 25000;").is_empty(),
        "above the sanctioned block"
    );
    assert!(
        found("let x = 49152;").is_empty(),
        "the auto-assign floor is out of range"
    );
    assert!(
        found("let x = 195000;").is_empty(),
        "six digits is not a port literal"
    );
    assert!(found("let x = x19500;").is_empty(), "part of an identifier");
    assert!(
        found("let x = 19500u16;").is_empty(),
        "a suffixed literal is not a bare port"
    );
    assert!(
        found("let x = 1_9500;").is_empty(),
        "an underscore-separated literal is not matched"
    );
}

/// Issue #1264: `tokio::spawn(server.run())` throws away the bind result. `AdminApiServer::run()` is
/// `bind()` then `join()` (and `ServerBuilder::run()` is `start()` then `join()`), so a failed bind
/// ends in a `JoinHandle` nobody reads, and the test's next request fails with `ConnectionRefused`
/// as if the product were broken. Bind first instead:
/// `let running = server.bind().await.expect("admin API binds");`, with the base URL from
/// `running.local_addr()`. Matched across lines (rustfmt splits a long builder chain) but never
/// across a `;` or a brace, so an `async move { ... .run().await }` block is not a match.
static DISCARDED_RUN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"spawn\(\s*[^;{}]*?\.run\(\)\s*,?\s*\)")
        .expect("discarded-run pattern is a valid constant regex")
});

/// Files allowed to spawn a server's `run()` future, with the reason. Empty: keep it that way.
const SPAWNED_RUN_EXCEPTIONS: &[(&str, &str)] = &[];

#[test]
fn no_test_discards_a_server_bind_result() {
    let exempt: Vec<&str> = SPAWNED_RUN_EXCEPTIONS.iter().map(|(f, _)| *f).collect();
    let mut offenders = Vec::new();
    for path in rust_sources() {
        let relative = rel(&path);
        if exempt.iter().any(|f| relative.ends_with(f)) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for found in DISCARDED_RUN.find_iter(&text) {
            let line = text[..found.start()].matches('\n').count() + 1;
            offenders.push(format!("  {relative}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "{} place(s) spawn a server's `run()` and discard its bind result; a failed bind then \
         surfaces as ConnectionRefused on the first request (issue #1264). Bind first: \
         `let running = server.bind().await.expect(\"admin API binds\");` and build the URL from \
         `running.local_addr()`:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

#[test]
fn the_discarded_run_matcher_accepts_spawned_runs_and_rejects_bound_servers() {
    for spawned in [
        "tokio::spawn(server.run());",
        "        tokio::spawn( admin.run() );",
        "let _ = tokio::spawn(server.run());",
        "tokio::spawn(AdminApiServer::new(addr, manager, None).run());",
        "tokio::spawn(ServerBuilder::from_cli(cli).manager(m.clone()).run());",
        "tokio::spawn(\n        ServerBuilder::from_cli(cli)\n            .manager(manager)\n            .run(),\n    );",
    ] {
        assert!(DISCARDED_RUN.is_match(spawned), "{spawned}");
    }
    for fine in [
        "let running = server.bind().await.expect(\"admin API binds\");",
        "tokio::spawn(async move { server.run().await })",
        "tokio::spawn(fut);\nlet x = server.run();",
        "tokio::spawn(running.join());",
        "let handle = runner.run();",
    ] {
        assert!(!DISCARDED_RUN.is_match(fine), "{fine}");
    }
}
