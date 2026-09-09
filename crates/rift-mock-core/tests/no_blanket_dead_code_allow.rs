//! Issue #1000: the library crates must not silence rustc's dead-code detector wholesale.
//!
//! A crate- or module-wide `#![allow(dead_code)]` is why #975 could leave ~6.4k lines unreachable
//! for nine months without a single warning. With the blankets gone,
//! `cargo clippy -- -D warnings` (already CI-enforced) is a permanent tripwire — but only for as
//! long as nobody puts one back, which is what this test is for.
//!
//! A genuinely-public item held by an out-of-tree embedder is still allowed to survive: annotate
//! *that item* with a targeted `#[allow(dead_code)]` and a one-line comment naming the consumer.
//! A targeted allow with a rationale is reviewable; a blanket one provably was not.
//!
//! The scan walks the source trees rather than checking a fixed file list, and matches any inner
//! attribute mentioning `dead_code` rather than one exact spelling. Both matter: a hardcoded list
//! cannot see a blanket allow added to a NEW module, and an exact-string match is defeated by the
//! combined form `#![allow(dead_code, unused_imports)]` or by
//! `#![cfg_attr(feature = "x", allow(dead_code))]` — each of which silences just as much.

use std::path::{Path, PathBuf};

/// Library source roots. `src/bin/` is skipped: those are binaries, not the library surface this
/// guard is about, and `bin/verify.rs` legitimately carries one.
fn library_src_roots() -> Vec<PathBuf> {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    vec![core.join("src"), core.join("../rift-http-proxy/src")]
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "bin") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Inner attributes (`#![...]`) that silence dead code. Inner attributes apply to the whole
/// enclosing file/module, so these are the blanket ones; an outer `#[allow(dead_code)]` on a
/// single item is deliberately NOT matched — that is the reviewable form this guard wants people
/// to use.
///
/// Matching is per-token rather than by substring, for two reasons that are easy to get wrong:
/// `dead_code` can arrive bundled (`allow(dead_code, unused_imports)`) so an exact-string match
/// misses it; and the lint **group** `unused` subsumes `dead_code` without containing its name,
/// so a substring search for "dead_code" misses `#![allow(unused)]` entirely. Conversely
/// `unused_imports` is a different lint that silences nothing here, so a naive "contains unused"
/// test would flag it wrongly.
fn blanket_allow_lines(src: &str) -> Vec<String> {
    src.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("#!["))
        .filter(|l| {
            l.replace(['#', '!', '[', ']', '(', ')', '"'], " ")
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == "dead_code" || tok == "unused")
        })
        .map(str::to_string)
        .collect()
}

#[test]
fn no_library_crate_or_module_silences_dead_code_wholesale() {
    let mut files = Vec::new();
    for root in library_src_roots() {
        rust_files(&root, &mut files);
    }
    assert!(
        files.len() > 50,
        "source scan found only {} files — the walk is broken and this guard would pass \
         vacuously against anything",
        files.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in blanket_allow_lines(&src) {
            let shown = path
                .to_string_lossy()
                .rsplit("/crates/")
                .next()
                .unwrap_or("?")
                .to_string();
            offenders.push(format!("{shown}: {line}"));
        }
    }

    assert!(
        offenders.is_empty(),
        "these library files carry a blanket inner `#![...dead_code...]` attribute, which switches \
         rustc's dead-code detector off for everything beneath them:\n  {}\n\
         That blanket is what let #975 leave thousands of unreachable lines without a warning. If \
         one item genuinely is public API an out-of-tree embedder holds, put a targeted \
         `#[allow(dead_code)]` on THAT item with a comment naming the consumer instead.",
        offenders.join("\n  ")
    );
}

/// The matcher itself, pinned — the guard above is only as good as what this recognises, and its
/// green state proves nothing on its own.
#[test]
fn the_matcher_recognises_every_blanket_spelling() {
    for silencing in [
        "#![allow(dead_code)]",
        "#![allow(dead_code, unused_imports)]",
        "#![allow(unused_imports, dead_code)]",
        "  #![allow(dead_code)]",
        "#![cfg_attr(feature = \"x\", allow(dead_code))]",
        // The `unused` lint GROUP subsumes `dead_code` without naming it — a substring search for
        // "dead_code" sails straight past this one.
        "#![allow(unused)]",
        "#![allow(unused, clippy::pedantic)]",
    ] {
        assert_eq!(
            blanket_allow_lines(silencing).len(),
            1,
            "must be recognised as a blanket allow: {silencing}"
        );
    }

    for benign in [
        "#[allow(dead_code)]",                       // outer: one item, reviewable
        "//! mentions dead_code in prose",           // a doc comment
        "#![allow(clippy::too_many_arguments)]",     // unrelated blanket
        "    #[allow(dead_code)] // named consumer", // targeted, indented
        // A different lint that silences no dead code — flagging it would be a false positive.
        "#![allow(unused_imports)]",
        "#![allow(unused_variables)]",
    ] {
        assert!(
            blanket_allow_lines(benign).is_empty(),
            "must NOT be flagged: {benign}"
        );
    }
}
