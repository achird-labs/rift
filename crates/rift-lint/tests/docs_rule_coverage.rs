//! Issue #942: the rules tables in `docs/features/linting.md` are the published contract for what
//! `rift-lint` reports, and they had fallen behind the validator — 7 of 11 warning codes and 1 of
//! 3 info codes had no row. A table that is only checked by eye drifts again the next time a rule
//! is added, so the comparison is made here instead.
//!
//! Issue #1008 widened it to error codes, which required scanning more than the validator:
//! `E001`/`E002` are emitted by `lib.rs` and `main.rs`, never by `validator.rs`. Scanning the
//! validator alone would have made the reverse-direction check below declare those two
//! documented-but-unemittable, which is the opposite of true.

/// Every source that can emit a code. All three are needed: `validator.rs` carries the rule
/// checks, while the two entry points own the read/parse and port-conflict codes.
const EMITTING_SOURCES: &[(&str, &str)] = &[
    ("validator.rs", include_str!("../src/validator.rs")),
    ("lib.rs", include_str!("../src/lib.rs")),
    ("main.rs", include_str!("../src/main.rs")),
];

/// The published tables.
const LINTING_DOCS: &str = include_str!("../../../docs/features/linting.md");

/// Every `"<prefix><3 digits>"` string literal in one source, deduplicated and sorted.
fn codes_in(source: &str, prefix: char) -> Vec<String> {
    let bytes: Vec<char> = source.chars().collect();
    let mut found: Vec<String> = Vec::new();

    for (i, c) in bytes.iter().enumerate() {
        if *c != prefix || i == 0 || bytes[i - 1] != '"' {
            continue;
        }
        let digits: String = bytes[i + 1..].iter().take(3).collect();
        if digits.len() == 3
            && digits.chars().all(|d| d.is_ascii_digit())
            && bytes.get(i + 4) == Some(&'"')
        {
            let code = format!("{prefix}{digits}");
            if !found.contains(&code) {
                found.push(code);
            }
        }
    }
    found.sort();
    found
}

/// Every code with `prefix` that any entry point can emit.
fn codes_with_prefix(prefix: char) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for (_, src) in EMITTING_SOURCES {
        for code in codes_in(src, prefix) {
            if !all.contains(&code) {
                all.push(code);
            }
        }
    }
    all.sort();
    all
}

/// A code is documented when it appears as the first cell of a markdown table row.
fn is_documented(code: &str) -> bool {
    LINTING_DOCS
        .lines()
        .any(|line| line.trim_start().starts_with(&format!("| {code} |")))
}

#[test]
fn every_warning_code_has_a_row_in_the_docs() {
    let codes = codes_with_prefix('W');
    assert!(
        !codes.is_empty(),
        "no W codes found in validator.rs — the extraction below is broken, not the docs"
    );

    let missing: Vec<&String> = codes.iter().filter(|c| !is_documented(c)).collect();
    assert!(
        missing.is_empty(),
        "docs/features/linting.md has no row for warning code(s): {missing:?}"
    );
}

#[test]
fn every_info_code_has_a_row_in_the_docs() {
    let codes = codes_with_prefix('I');
    assert!(
        !codes.is_empty(),
        "no I codes found in validator.rs — the extraction below is broken, not the docs"
    );

    let missing: Vec<&String> = codes.iter().filter(|c| !is_documented(c)).collect();
    assert!(
        missing.is_empty(),
        "docs/features/linting.md has no row for info code(s): {missing:?}"
    );
}

/// The inverse drift: a row for a code nothing can emit any more is just as misleading as a
/// missing one, and is what a rule rename leaves behind. `E012` is deliberately absent from both
/// sides — it is an unassigned number, so it must appear in neither the sources nor the docs.
#[test]
fn the_docs_do_not_document_codes_that_no_longer_exist() {
    let mut known = codes_with_prefix('W');
    known.extend(codes_with_prefix('I'));
    known.extend(codes_with_prefix('E'));

    let stale: Vec<String> = LINTING_DOCS
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| ")?;
            let code = cell.split(" |").next()?.trim();
            let is_wi = code.len() == 4
                && (code.starts_with('W') || code.starts_with('I') || code.starts_with('E'))
                && code[1..].chars().all(|d| d.is_ascii_digit());
            (is_wi && !known.contains(&code.to_string())).then(|| code.to_string())
        })
        .collect();

    assert!(
        stale.is_empty(),
        "docs/features/linting.md documents code(s) no entry point can emit: {stale:?}"
    );
}

#[test]
fn every_error_code_has_a_row_in_the_docs() {
    let codes = codes_with_prefix('E');
    assert!(
        !codes.is_empty(),
        "no E codes found across the emitting sources — the extraction is broken, not the docs"
    );

    let missing: Vec<&String> = codes.iter().filter(|c| !is_documented(c)).collect();
    assert!(
        missing.is_empty(),
        "docs/features/linting.md has no row for error code(s): {missing:?}"
    );
}

/// `E001` and `E002` are emitted only outside `validator.rs`, so this pins the multi-source scan
/// itself: if a refactor moved the entry points or the source list went stale, the coverage tests
/// above would silently start passing for the wrong reason — an empty or truncated code set.
#[test]
fn the_scan_reaches_the_codes_that_live_outside_the_validator() {
    let all = codes_with_prefix('E');
    for code in ["E001", "E002"] {
        assert!(
            all.contains(&code.to_string()),
            "{code} is emitted by an entry point but the scan did not see it: {all:?}"
        );
    }
    assert!(
        !codes_in(EMITTING_SOURCES[0].1, 'E').contains(&"E002".to_string()),
        "E002 is not a validator code; if it is now, this test's premise needs revisiting"
    );
}

/// Every code handed to a `LintIssue` constructor, paired with the constructor's severity.
///
/// The code is the first string literal after `LintIssue::error(` / `warning(` / `info(`, which may
/// sit on the next line (rustfmt splits long calls).
fn constructed_codes(source: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (kind, ctor) in [
        ("error", "LintIssue::error("),
        ("warning", "LintIssue::warning("),
        ("info", "LintIssue::info("),
    ] {
        let mut rest = source;
        while let Some(at) = rest.find(ctor) {
            rest = &rest[at + ctor.len()..];
            let arg = rest.trim_start();
            if let Some(stripped) = arg.strip_prefix('"')
                && let Some(end) = stripped.find('"')
            {
                out.push((kind, stripped[..end].to_string()));
            }
        }
    }
    out
}

/// Issue #1156: `E042` was born as `LintIssue::warning("E042", …)` — an error code with warning
/// severity — so a consumer filtering on the `E` prefix and one filtering on severity saw different
/// rules. It was the only mismatch. This makes the invariant structural: the letter IS the severity.
#[test]
fn every_code_prefix_matches_its_constructor_severity() {
    let mut mismatched = Vec::new();
    let mut seen = 0;
    for (file, src) in EMITTING_SOURCES {
        for (kind, code) in constructed_codes(src) {
            seen += 1;
            let want = match kind {
                "error" => 'E',
                "warning" => 'W',
                _ => 'I',
            };
            if !code.starts_with(want) {
                mismatched.push(format!("{file}: LintIssue::{kind}(\"{code}\")"));
            }
        }
    }
    assert!(
        seen > 20,
        "found only {seen} constructor calls — the scan is broken, not the code"
    );
    assert!(
        mismatched.is_empty(),
        "a code's letter must match its severity (E=error, W=warning, I=info): {mismatched:?}"
    );
}

/// Retired numbers are never reused, so a stale reference or a revived number is caught: `E012` was
/// never assigned, and `E042` was renumbered to `W014` (issue #1156). Neither may appear as an
/// emitted code or as a documented rule.
#[test]
fn retired_codes_are_neither_emitted_nor_documented() {
    for retired in ["E012", "E042"] {
        let emitted: Vec<&str> = EMITTING_SOURCES
            .iter()
            .filter(|(_, src)| codes_in(src, 'E').contains(&retired.to_string()))
            .map(|(file, _)| *file)
            .collect();
        assert!(
            emitted.is_empty(),
            "{retired} is retired but still emitted by {emitted:?}"
        );
        assert!(
            !is_documented(retired),
            "{retired} is retired but still has a rule row in linting.md"
        );
    }
}
