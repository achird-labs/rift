//! What one generated case's report means, and the signature a divergence is filed under.

use rift_differential::allow::{self, Rule};
use rift_differential::driver::CaseReport;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Both engines agreed, up to the Rift-wide (`*` case) allow-list entries.
    Agree,
    /// The engines answered differently: the unexplained difference locations.
    Diverge(BTreeSet<String>),
    /// The run itself broke (port allocation, an unreadable list, a failed cleanup, or neither
    /// engine listening). Never a divergence and never shrunk: the run stops.
    Infrastructure(Vec<String>),
}

/// Classifies `report` against `rules`. Only the `*` entries can match a generated case's name,
/// and none is ever reported stale here: the stale-entry check belongs to the committed cases.
#[must_use]
pub fn of(report: &CaseReport, rules: &[Rule]) -> Verdict {
    if !report.errors.is_empty() || !report.unreachable.is_empty() {
        let mut why = report.errors.clone();
        why.extend(
            report
                .unreachable
                .iter()
                .map(|at| format!("{at}: neither engine accepted the connection")),
        );
        return Verdict::Infrastructure(why);
    }
    let mut used = vec![0usize; rules.len()];
    let unexplained = allow::classify(&report.name, &report.differences, rules, &mut used);
    if unexplained.is_empty() {
        Verdict::Agree
    } else {
        Verdict::Diverge(unexplained.iter().map(|d| d.location.clone()).collect())
    }
}

/// `location` with every index (`[12]`, `/3/`) replaced by `*`: where a divergence is, not which
/// step or stub of one random case it surfaced at.
#[must_use]
pub fn shape(location: &str) -> String {
    let mut out = String::with_capacity(location.len());
    let mut chars = location.chars().peekable();
    let mut segment_start = true;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() && segment_start {
            let mut digits = c.to_string();
            while let Some(d) = chars.next_if(char::is_ascii_digit) {
                digits.push(d);
            }
            // A whole segment of digits (`/3/`, `[12]`) is an index; `p0` or `#1` is not.
            if matches!(chars.peek(), None | Some('/' | ']' | '.')) {
                out.push('*');
            } else {
                out.push_str(&digits);
            }
            segment_start = false;
            continue;
        }
        segment_start = matches!(c, '/' | '[');
        out.push(c);
    }
    out
}

/// The de-duplication key: the sorted, de-indexed locations, FNV-1a hashed to 8 hex digits.
#[must_use]
pub fn signature(locations: &BTreeSet<String>) -> String {
    let shapes: BTreeSet<String> = locations.iter().map(|l| shape(l)).collect();
    let mut hash: u32 = 0x811c_9dc5;
    // One location per line, so `a` + `bc` and `ab` + `c` hash apart.
    for line in &shapes {
        for byte in line.bytes().chain(std::iter::once(b'\n')) {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    format!("{hash:08x}")
}
