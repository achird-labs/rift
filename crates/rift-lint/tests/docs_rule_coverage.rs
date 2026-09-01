//! Issue #942: the rules tables in `docs/features/linting.md` are the published contract for what
//! `rift-lint` reports, and they had fallen behind the validator — 7 of 11 warning codes and 1 of
//! 3 info codes had no row. A table that is only checked by eye drifts again the next time a rule
//! is added, so the comparison is made here instead.
//!
//! Scoped to warning and info codes. The error table is a larger, separate gap (most `E0xx` codes
//! are still undocumented) and is tracked on its own issue; widening this test to `E` codes is the
//! natural way to close it, and is the reason the code prefixes are a parameter below.

/// The validator's own source. Codes are string literals at the `LintIssue::{warning,info}` call
/// sites, so the set of codes it *can* emit is derivable from the source rather than from a list
/// kept in parallel with it.
const VALIDATOR_RS: &str = include_str!("../src/validator.rs");

/// The published tables.
const LINTING_DOCS: &str = include_str!("../../../docs/features/linting.md");

/// Every `"<prefix><3 digits>"` string literal in `source`, deduplicated and sorted.
fn codes_with_prefix(source: &str, prefix: char) -> Vec<String> {
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

/// A code is documented when it appears as the first cell of a markdown table row.
fn is_documented(code: &str) -> bool {
    LINTING_DOCS
        .lines()
        .any(|line| line.trim_start().starts_with(&format!("| {code} |")))
}

#[test]
fn every_warning_code_has_a_row_in_the_docs() {
    let codes = codes_with_prefix(VALIDATOR_RS, 'W');
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
    let codes = codes_with_prefix(VALIDATOR_RS, 'I');
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

/// The inverse drift: a row for a code the validator can no longer emit is just as misleading as a
/// missing one, and is what a rule rename leaves behind.
#[test]
fn the_docs_do_not_document_warning_or_info_codes_that_no_longer_exist() {
    let mut known = codes_with_prefix(VALIDATOR_RS, 'W');
    known.extend(codes_with_prefix(VALIDATOR_RS, 'I'));

    let stale: Vec<String> = LINTING_DOCS
        .lines()
        .filter_map(|line| {
            let cell = line.trim_start().strip_prefix("| ")?;
            let code = cell.split(" |").next()?.trim();
            let is_wi = code.len() == 4
                && (code.starts_with('W') || code.starts_with('I'))
                && code[1..].chars().all(|d| d.is_ascii_digit());
            (is_wi && !known.contains(&code.to_string())).then(|| code.to_string())
        })
        .collect();

    assert!(
        stale.is_empty(),
        "docs/features/linting.md documents code(s) the validator cannot emit: {stale:?}"
    );
}
