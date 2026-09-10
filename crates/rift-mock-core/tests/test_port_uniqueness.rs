//! Issue #1036: every fixed test port belongs to exactly one file.
//!
//! # Why this is a real failure mode, and why it is invisible
//!
//! The issue that prompted this guard blamed `cargo test` for running test *binaries*
//! concurrently. It does not — measured: three `tests/*.rs` binaries each sleeping 3s ran strictly
//! back-to-back. Cargo parallelises `#[test]` functions *within* a binary, never across binaries.
//! Two mechanisms do produce real collisions:
//!
//! 1. **Concurrent `cargo` invocations on one machine.** `ship-issues` worktrees building side by
//!    side, or `--lib` and `--tests` in two terminals — which is also how CI runs them, on
//!    different runners. This is what bit #999.
//! 2. **Two tests in one file sharing a literal**, since those *do* run on a thread pool together.
//!
//! Neither announces itself. Imposter listeners bind with `SO_REUSEADDR` **and** `SO_REUSEPORT`
//! (`proxy/network.rs`), and the in-manager duplicate check is per-`ImposterManager`, so two binds
//! on one port both succeed and the kernel load-balances between them. The loser does not get
//! `AddrInUse`; it gets the *other* imposter's response, and fails on a decode or a status
//! mismatch that points nowhere near the real cause. CI being green is not evidence a port is free
//! — which is exactly why this has to be a static check rather than something a test run finds.
//!
//! # Scope
//!
//! This enforces **cross-file** uniqueness only. Two tests in the *same* file sharing a literal is
//! mechanism (2) above and is deliberately not caught here: a port in a shared helper is
//! legitimate, and there is no way to tell the two apart from the literal alone. That stays a
//! review rule.
//!
//! Modelled on `no_blanket_dead_code_allow.rs`, including its anti-vacuity floors — a guard whose
//! matcher silently stopped matching would pass forever.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The sanctioned block for fixed test ports.
///
/// Bounded **below** `find_available_port`'s 49152 floor so a fixed port can never be handed out
/// as an auto-assigned one, and above the privileged range.
const PORT_LO: u16 = 15000;
const PORT_HI: u16 = 24999;

/// Blocks owned by one file because it derives ports from a counter base rather than writing each
/// literal — so a literal from another file landing inside one is a collision the plain
/// one-file-per-port rule cannot see.
const RESERVED: &[(u16, u16, &str)] = &[
    (18000, 18021, "rift_extensions.rs"),
    (19000, 19091, "mountebank_compatibility.rs"),
];

/// Ports allowed in more than one file, with the reason. Keep this list short: a growing one means
/// the rule is not being followed rather than that the rule is wrong.
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
            let before_ok = start == 0 || !(lb[start - 1].is_ascii_alphanumeric() || lb[start - 1] == b'_');
            let after_ok = i == lb.len() || !(lb[i].is_ascii_alphanumeric() || lb[i] == b'_');
            if before_ok && after_ok && i - start == 5 {
                if let Ok(n) = line[start..i].parse::<u16>() {
                    if (PORT_LO..=PORT_HI).contains(&n) {
                        out.push((n, lineno + 1));
                    }
                }
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
fn every_fixed_test_port_belongs_to_exactly_one_file() {
    let files = rust_sources();
    let mut owners: HashMap<u16, Vec<(String, usize)>> = HashMap::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read source");
        for (port, line) in port_literals(&text) {
            let entry = owners.entry(port).or_default();
            if !entry.iter().any(|(f, _)| f == &rel(path)) {
                entry.push((rel(path), line));
            }
        }
    }

    // Anti-vacuity: if the walk or the matcher silently stopped working, every assertion below
    // would pass on an empty set. These floors are well under the real numbers (219 files, 395
    // ports at the time of writing) so ordinary churn cannot trip them.
    assert!(
        files.len() > 100,
        "expected to scan >100 Rust sources, found {} — the walk is broken, and a guard that \
         scans nothing passes forever",
        files.len()
    );
    assert!(
        owners.len() > 200,
        "expected >200 distinct port literals, found {} — the matcher is broken",
        owners.len()
    );

    let exempt: HashMap<u16, &str> = EXCEPTIONS
        .iter()
        .map(|(p, why)| (p.parse().expect("exception port parses"), *why))
        .collect();

    let mut shared: Vec<String> = owners
        .iter()
        .filter(|(port, files)| files.len() > 1 && !exempt.contains_key(port))
        .map(|(port, files)| {
            let where_ = files
                .iter()
                .map(|(f, l)| format!("{f}:{l}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("  {port}: {where_}")
        })
        .collect();
    shared.sort();

    assert!(
        shared.is_empty(),
        "these fixed ports appear in more than one file, so two concurrent `cargo` runs can bind \
         each one twice — and SO_REUSEPORT means neither gets AddrInUse, they just serve each \
         other's traffic:\n{}\n\nGive each file its own port, or prefer an auto-assigned one: \
         `create_imposter` returns the bound port, and the listeners expose `local_addr()`.",
        shared.join("\n")
    );

    // A literal inside a counter-owned block is a collision even if it appears in only one other
    // file, because the owning file's ports are computed rather than written.
    let mut trespass: Vec<String> = Vec::new();
    for (port, locations) in &owners {
        for (lo, hi, owner) in RESERVED {
            if (*lo..=*hi).contains(port) {
                for (file, line) in locations {
                    if !file.ends_with(owner) {
                        trespass.push(format!(
                            "  {port} at {file}:{line} is inside {lo}-{hi}, owned by {owner}"
                        ));
                    }
                }
            }
        }
    }
    trespass.sort();
    assert!(
        trespass.is_empty(),
        "these literals fall inside a range another file allocates from a counter:\n{}",
        trespass.join("\n")
    );
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

    assert_eq!(found("Duration::from_millis(20000)"), vec![20000],
        "a duration that happens to look like a port IS matched — the matcher cannot tell, which \
         is why the exception list exists rather than a cleverer regex");

    assert!(found("let x = 14999;").is_empty(), "below the sanctioned block");
    assert!(found("let x = 25000;").is_empty(), "above the sanctioned block");
    assert!(found("let x = 49152;").is_empty(), "the auto-assign floor is out of range");
    assert!(found("let x = 195000;").is_empty(), "six digits is not a port literal");
    assert!(found("let x = x19500;").is_empty(), "part of an identifier");
    assert!(found("let x = 19500u16;").is_empty(), "a suffixed literal is not a bare port");
    assert!(found("let x = 1_9500;").is_empty(), "an underscore-separated literal is not matched");
}
