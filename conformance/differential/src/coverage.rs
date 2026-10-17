//! The Mountebank-coverage ratchet (issue #1548): how much of the oracle the cases exercise.
//!
//! The differential run can execute Mountebank under `NODE_V8_COVERAGE`, and `c8 report` turns the
//! profile into a `coverage-summary.json`. [`compare`] holds that summary against the checked-in
//! `mb-coverage.json`: every Mountebank source file c8 reports is either `excluded` (with the
//! reason it cannot or need not be reached) or has a `baseline` of covered/total lines, branches
//! and functions. The check is strict both ways. A drop means a case stopped reaching code it used
//! to; a rise means the baseline is slack and must be blessed, so a gap-closing change proves the
//! gap closed. Counts, never percentages: totals move with a Mountebank release, and V8 does not
//! count the branches of a function that never ran, so a percentage can rise while coverage falls.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// `[covered, total]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count(pub u64, pub u64);

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.0, self.1)
    }
}

/// One file's covered/total counts, in c8's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCoverage {
    pub lines: Count,
    pub branches: Count,
    pub functions: Count,
}

impl FileCoverage {
    fn metrics(&self) -> [(Metric, Count); 3] {
        [
            (Metric::Lines, self.lines),
            (Metric::Branches, self.branches),
            (Metric::Functions, self.functions),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Lines,
    Branches,
    Functions,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Metric::Lines => "lines",
            Metric::Branches => "branches",
            Metric::Functions => "functions",
        })
    }
}

/// `mb-coverage.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// The Mountebank version the baseline was measured on.
    pub mountebank: String,
    /// The node major it was measured on: V8's block ranges can shift between majors.
    pub node: String,
    /// The c8 version `scripts/mb-coverage.sh` reports with.
    pub c8: String,
    /// Path (relative to the Mountebank package, `src/...`) or `<dir>/**` → why it is not tracked.
    pub excluded: BTreeMap<String, String>,
    pub baseline: BTreeMap<String, FileCoverage>,
}

/// Which Mountebank and node produced a profile: Mountebank's own `GET /config` (`version`,
/// `process.nodeVersion`), recorded by the harness next to the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub mountebank: String,
    /// As node reports it: `v22.23.3`.
    pub node: String,
    /// The Mountebank package directory; c8 reports absolute paths below it.
    pub root: String,
}

impl Run {
    /// `v22.23.3` → `22`.
    #[must_use]
    pub fn node_major(&self) -> &str {
        let version = self.node.trim_start_matches('v');
        version.split('.').next().unwrap_or(version)
    }
}

/// Why the summary and the baseline disagree. Every variant fails the ratchet; `--bless` settles
/// all but the classification ones (`Unclassified`, `ClassifiedTwice`, `StaleExclusion`), which
/// only a person editing `mb-coverage.json` can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The profile came from another node major; nothing else is compared.
    NodeMajor { baseline: String, run: String },
    /// Another Mountebank release: expect file changes too.
    MountebankVersion { baseline: String, run: String },
    /// A file c8 reports that is neither excluded nor baselined (a Mountebank bump added it).
    Unclassified(String),
    /// A file both baselined and excluded, or excluded by more than one pattern.
    ClassifiedTwice(String),
    /// A baselined file c8 no longer reports.
    Missing(String),
    /// An `excluded` pattern that matches no reported file.
    StaleExclusion(String),
    /// Fewer covered: a case stopped reaching this code.
    Dropped {
        file: String,
        metric: Metric,
        was: Count,
        now: Count,
    },
    /// More covered: the baseline is slack.
    Rose {
        file: String,
        metric: Metric,
        was: Count,
        now: Count,
    },
    /// Same covered, different total: the file itself changed.
    Retotalled {
        file: String,
        metric: Metric,
        was: Count,
        now: Count,
    },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::NodeMajor { baseline, run } => write!(
                f,
                "the profile is from node {run} but the baseline was measured on node {baseline}; \
                 V8 coverage is only comparable within one major. Run under node {baseline} (what \
                 CI pins), or re-measure everything with `scripts/mb-coverage.sh --bless` on the \
                 new major and commit the baseline"
            ),
            Change::MountebankVersion { baseline, run } => write!(
                f,
                "the profile is from Mountebank {run} but the baseline is for {baseline}: \
                 classify any new file, then `scripts/mb-coverage.sh --bless`"
            ),
            Change::Unclassified(file) => write!(
                f,
                "{file}: not classified — in mb-coverage.json, either add it to `excluded` with a \
                 reason, or add `{file:?}: {{\"lines\": [0, 0], \"branches\": [0, 0], \
                 \"functions\": [0, 0]}}` to `baseline` and run `scripts/mb-coverage.sh --bless`"
            ),
            Change::ClassifiedTwice(file) => write!(
                f,
                "{file}: classified more than once in mb-coverage.json (baseline and/or several \
                 `excluded` patterns); keep exactly one"
            ),
            Change::Missing(file) => write!(
                f,
                "{file}: baselined but not reported (removed by a Mountebank release?)"
            ),
            Change::StaleExclusion(pattern) => {
                write!(f, "excluded {pattern:?} matches no reported file")
            }
            Change::Dropped {
                file,
                metric,
                was,
                now,
            } => write!(
                f,
                "{file}: {metric} dropped {was} → {now}: a case no longer reaches code it did"
            ),
            Change::Rose {
                file,
                metric,
                was,
                now,
            } => write!(
                f,
                "{file}: {metric} rose {was} → {now}: run `scripts/mb-coverage.sh --bless` and \
                 commit the baseline"
            ),
            Change::Retotalled {
                file,
                metric,
                was,
                now,
            } => write!(
                f,
                "{file}: {metric} total changed {was} → {now} at the same covered count"
            ),
        }
    }
}

/// Whether `pattern` names `file`: an exact path, or `<dir>/**` for everything below `<dir>`.
#[must_use]
pub fn pattern_matches(pattern: &str, file: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(dir) => file
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/')),
        None => pattern == file,
    }
}

/// c8's `coverage-summary.json` as `src/...` → counts. The `total` row is dropped.
///
/// # Errors
/// When the document is not a summary, or names a file outside `root` (c8 was run against
/// another package than the one the harness profiled).
pub fn parse_summary(
    summary: &Value,
    root: &str,
) -> Result<BTreeMap<String, FileCoverage>, String> {
    let rows = summary
        .as_object()
        .ok_or("coverage summary is not a JSON object")?;
    let root = root.trim_end_matches('/');
    let mut files = BTreeMap::new();
    for (path, row) in rows {
        if path == "total" {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(|| format!("{path} is not below the Mountebank package {root}"))?;
        let count = |metric: &str| -> Result<Count, String> {
            let field = |name: &str| {
                row.get(metric)
                    .and_then(|m| m.get(name))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("{path}: no {metric}.{name}"))
            };
            Ok(Count(field("covered")?, field("total")?))
        };
        files.insert(
            relative.to_string(),
            FileCoverage {
                lines: count("lines")?,
                branches: count("branches")?,
                functions: count("functions")?,
            },
        );
    }
    Ok(files)
}

/// Every way `summary` (from `run`) differs from `baseline`; empty when it matches exactly.
#[must_use]
pub fn compare(
    baseline: &Baseline,
    run: &Run,
    summary: &BTreeMap<String, FileCoverage>,
) -> Vec<Change> {
    if run.node_major() != baseline.node {
        return vec![Change::NodeMajor {
            baseline: baseline.node.clone(),
            run: run.node.clone(),
        }];
    }
    let mut changes = Vec::new();
    if run.mountebank != baseline.mountebank {
        changes.push(Change::MountebankVersion {
            baseline: baseline.mountebank.clone(),
            run: run.mountebank.clone(),
        });
    }
    changes.extend(classification(baseline, summary));
    for (file, was) in &baseline.baseline {
        let Some(now) = summary.get(file) else {
            changes.push(Change::Missing(file.clone()));
            continue;
        };
        for ((metric, was), (_, now)) in was.metrics().into_iter().zip(now.metrics()) {
            let file = file.clone();
            match was.0.cmp(&now.0) {
                std::cmp::Ordering::Greater => changes.push(Change::Dropped {
                    file,
                    metric,
                    was,
                    now,
                }),
                std::cmp::Ordering::Less => changes.push(Change::Rose {
                    file,
                    metric,
                    was,
                    now,
                }),
                std::cmp::Ordering::Equal if was.1 != now.1 => changes.push(Change::Retotalled {
                    file,
                    metric,
                    was,
                    now,
                }),
                std::cmp::Ordering::Equal => {}
            }
        }
    }
    changes
}

/// Files classified zero or several times, and exclusions that match nothing.
fn classification(baseline: &Baseline, summary: &BTreeMap<String, FileCoverage>) -> Vec<Change> {
    let mut changes = Vec::new();
    for file in summary.keys() {
        let exclusions = baseline
            .excluded
            .keys()
            .filter(|pattern| pattern_matches(pattern, file))
            .count();
        match exclusions + usize::from(baseline.baseline.contains_key(file)) {
            0 => changes.push(Change::Unclassified(file.clone())),
            1 => {}
            _ => changes.push(Change::ClassifiedTwice(file.clone())),
        }
    }
    for pattern in baseline.excluded.keys() {
        if !summary.keys().any(|file| pattern_matches(pattern, file)) {
            changes.push(Change::StaleExclusion(pattern.clone()));
        }
    }
    changes
}

/// The baseline re-measured from `summary`: the counts of every file not excluded, and the run's
/// Mountebank version and node major.
///
/// # Errors
/// The classification problems, which only a person editing `mb-coverage.json` can settle.
pub fn bless(
    baseline: &Baseline,
    run: &Run,
    summary: &BTreeMap<String, FileCoverage>,
) -> Result<Baseline, Vec<Change>> {
    let problems = classification(baseline, summary);
    if !problems.is_empty() {
        return Err(problems);
    }
    let measured = summary
        .iter()
        .filter(|(file, _)| baseline.baseline.contains_key(*file))
        .map(|(file, coverage)| (file.clone(), *coverage))
        .collect();
    Ok(Baseline {
        mountebank: run.mountebank.clone(),
        node: run.node_major().to_string(),
        c8: baseline.c8.clone(),
        excluded: baseline.excluded.clone(),
        baseline: measured,
    })
}

/// The sum over the baselined files.
#[must_use]
pub fn in_scope_total(files: &BTreeMap<String, FileCoverage>) -> FileCoverage {
    let zero = FileCoverage {
        lines: Count(0, 0),
        branches: Count(0, 0),
        functions: Count(0, 0),
    };
    files.values().fold(zero, |acc, f| {
        let add = |a: Count, b: Count| Count(a.0 + b.0, a.1 + b.1);
        FileCoverage {
            lines: add(acc.lines, f.lines),
            branches: add(acc.branches, f.branches),
            functions: add(acc.functions, f.functions),
        }
    })
}

/// `mb-coverage.json` text: one line per file, so a bless diff shows exactly the files that moved.
#[must_use]
pub fn render(baseline: &Baseline) -> String {
    let quote = |s: &str| serde_json::to_string(s).expect("a string always serialises");
    let mut out = String::from("{\n");
    for (key, value) in [
        ("mountebank", &baseline.mountebank),
        ("node", &baseline.node),
        ("c8", &baseline.c8),
    ] {
        out.push_str(&format!("  {}: {},\n", quote(key), quote(value)));
    }
    out.push_str("  \"excluded\": {\n");
    let excluded: Vec<String> = baseline
        .excluded
        .iter()
        .map(|(pattern, reason)| format!("    {}: {}", quote(pattern), quote(reason)))
        .collect();
    out.push_str(&excluded.join(",\n"));
    out.push_str("\n  },\n  \"baseline\": {\n");
    let files: Vec<String> = baseline
        .baseline
        .iter()
        .map(|(file, c)| {
            format!(
                "    {}: {{\"lines\": [{}, {}], \"branches\": [{}, {}], \"functions\": [{}, {}]}}",
                quote(file),
                c.lines.0,
                c.lines.1,
                c.branches.0,
                c.branches.1,
                c.functions.0,
                c.functions.1
            )
        })
        .collect();
    out.push_str(&files.join(",\n"));
    out.push_str("\n  }\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cov(lines: (u64, u64), branches: (u64, u64), functions: (u64, u64)) -> FileCoverage {
        FileCoverage {
            lines: Count(lines.0, lines.1),
            branches: Count(branches.0, branches.1),
            functions: Count(functions.0, functions.1),
        }
    }

    fn baseline() -> Baseline {
        Baseline {
            mountebank: "2.9.5".to_string(),
            node: "22".to_string(),
            c8: "10.1.3".to_string(),
            excluded: BTreeMap::from([(
                "src/models/tcp/**".to_string(),
                "no tcp protocol".to_string(),
            )]),
            baseline: BTreeMap::from([
                (
                    "src/models/predicates.js".to_string(),
                    cov((400, 438), (58, 65), (33, 34)),
                ),
                (
                    "src/models/https/httpsServer.js".to_string(),
                    cov((14, 42), (1, 1), (0, 2)),
                ),
            ]),
        }
    }

    fn run() -> Run {
        Run {
            mountebank: "2.9.5".to_string(),
            node: "v22.23.3".to_string(),
            root: "/mb".to_string(),
        }
    }

    fn summary() -> BTreeMap<String, FileCoverage> {
        let mut files = baseline().baseline;
        files.insert(
            "src/models/tcp/tcpServer.js".to_string(),
            cov((20, 156), (1, 1), (0, 9)),
        );
        files
    }

    #[test]
    fn an_identical_summary_has_no_changes() {
        assert_eq!(compare(&baseline(), &run(), &summary()), vec![]);
    }

    #[test]
    fn a_drop_fails() {
        let mut now = summary();
        now.insert(
            "src/models/predicates.js".to_string(),
            cov((398, 438), (58, 65), (33, 34)),
        );
        assert_eq!(
            compare(&baseline(), &run(), &now),
            vec![Change::Dropped {
                file: "src/models/predicates.js".to_string(),
                metric: Metric::Lines,
                was: Count(400, 438),
                now: Count(398, 438),
            }]
        );
    }

    #[test]
    fn a_rise_fails_and_says_to_bless() {
        let mut now = summary();
        now.insert(
            "src/models/https/httpsServer.js".to_string(),
            cov((38, 42), (1, 1), (2, 2)),
        );
        let changes = compare(&baseline(), &run(), &now);
        assert_eq!(changes.len(), 2, "lines and functions rose: {changes:?}");
        assert!(matches!(
            &changes[0],
            Change::Rose {
                metric: Metric::Lines,
                was: Count(14, 42),
                now: Count(38, 42),
                ..
            }
        ));
        assert!(changes[0].to_string().contains("--bless"));

        let blessed = bless(&baseline(), &run(), &now).expect("a rise is blessable");
        assert_eq!(
            blessed.baseline["src/models/https/httpsServer.js"],
            cov((38, 42), (1, 1), (2, 2))
        );
        assert_eq!(compare(&blessed, &run(), &now), vec![]);
    }

    #[test]
    fn a_new_file_from_a_mountebank_bump_must_be_classified() {
        let mut now = summary();
        now.insert(
            "src/models/grpc/grpcServer.js".to_string(),
            cov((0, 10), (0, 0), (0, 1)),
        );
        let bumped = Run {
            mountebank: "2.9.6".to_string(),
            ..run()
        };
        let changes = compare(&baseline(), &bumped, &now);
        assert_eq!(
            changes,
            vec![
                Change::MountebankVersion {
                    baseline: "2.9.5".to_string(),
                    run: "2.9.6".to_string()
                },
                Change::Unclassified("src/models/grpc/grpcServer.js".to_string()),
            ]
        );
        assert_eq!(
            bless(&baseline(), &bumped, &now),
            Err(vec![Change::Unclassified(
                "src/models/grpc/grpcServer.js".to_string()
            )]),
            "bless records counts; it does not classify"
        );
    }

    #[test]
    fn a_file_classified_twice_fails() {
        let mut twice = baseline();
        twice.baseline.insert(
            "src/models/tcp/tcpServer.js".to_string(),
            cov((20, 156), (1, 1), (0, 9)),
        );
        let changes = compare(&twice, &run(), &summary());
        assert_eq!(
            changes,
            vec![Change::ClassifiedTwice(
                "src/models/tcp/tcpServer.js".to_string()
            )]
        );
        assert!(bless(&twice, &run(), &summary()).is_err());
    }

    #[test]
    fn a_node_major_mismatch_refuses_to_compare() {
        let node26 = Run {
            node: "v26.10.0".to_string(),
            ..run()
        };
        let mut now = summary();
        now.insert(
            "src/models/predicates.js".to_string(),
            cov((1, 438), (1, 65), (1, 34)),
        );
        let changes = compare(&baseline(), &node26, &now);
        assert_eq!(
            changes,
            vec![Change::NodeMajor {
                baseline: "22".to_string(),
                run: "v26.10.0".to_string()
            }],
            "nothing else is compared across majors"
        );
        let message = changes[0].to_string();
        assert!(
            message.contains("node 22") && message.contains("--bless"),
            "{message}"
        );
        assert_eq!(
            bless(&baseline(), &node26, &summary()).map(|b| b.node),
            Ok("26".to_string()),
            "blessing on another major re-measures for it"
        );
    }

    #[test]
    fn a_missing_file_and_a_stale_exclusion_fail() {
        let mut now = summary();
        now.remove("src/models/https/httpsServer.js");
        now.remove("src/models/tcp/tcpServer.js");
        assert_eq!(
            compare(&baseline(), &run(), &now),
            vec![
                Change::StaleExclusion("src/models/tcp/**".to_string()),
                Change::Missing("src/models/https/httpsServer.js".to_string()),
            ]
        );
    }

    #[test]
    fn a_changed_total_at_the_same_coverage_fails() {
        let mut now = summary();
        now.insert(
            "src/models/predicates.js".to_string(),
            cov((400, 440), (58, 65), (33, 34)),
        );
        assert!(matches!(
            compare(&baseline(), &run(), &now).as_slice(),
            [Change::Retotalled {
                metric: Metric::Lines,
                ..
            }]
        ));
    }

    #[test]
    fn patterns_name_a_file_or_a_directory() {
        assert!(pattern_matches(
            "src/models/tcp/**",
            "src/models/tcp/tcpServer.js"
        ));
        assert!(pattern_matches("src/cli/**", "src/cli/a/b.js"));
        assert!(!pattern_matches(
            "src/models/tcp/**",
            "src/models/tcpish.js"
        ));
        assert!(pattern_matches("src/util/logger.js", "src/util/logger.js"));
        assert!(!pattern_matches(
            "src/util/logger.js",
            "src/util/scopedLogger.js"
        ));
    }

    #[test]
    fn a_summary_is_read_relative_to_the_package() {
        let row = json!({
            "lines": {"total": 42, "covered": 14, "skipped": 0, "pct": 33.33},
            "statements": {"total": 42, "covered": 14, "skipped": 0, "pct": 33.33},
            "functions": {"total": 2, "covered": 0, "skipped": 0, "pct": 0},
            "branches": {"total": 1, "covered": 1, "skipped": 0, "pct": 100}
        });
        let summary = json!({"total": row, "/mb/src/models/https/httpsServer.js": row});
        assert_eq!(
            parse_summary(&summary, "/mb/").expect("parses"),
            BTreeMap::from([(
                "src/models/https/httpsServer.js".to_string(),
                cov((14, 42), (1, 1), (0, 2))
            )])
        );
        let elsewhere = json!({"/other/src/x.js": row});
        assert!(parse_summary(&elsewhere, "/mb").is_err());
        assert!(parse_summary(&json!({"/mb/src/x.js": {"lines": {}}}), "/mb").is_err());
    }

    #[test]
    fn render_round_trips() {
        let text = render(&baseline());
        let parsed: Baseline = serde_json::from_str(&text).expect("rendered baseline parses");
        assert_eq!(parsed, baseline());
        assert!(text.contains(
            "\"src/models/predicates.js\": {\"lines\": [400, 438], \"branches\": [58, 65], \
             \"functions\": [33, 34]}"
        ));
    }

    #[test]
    fn the_in_scope_total_sums_the_files() {
        assert_eq!(
            in_scope_total(&baseline().baseline),
            cov((414, 480), (59, 66), (33, 36))
        );
    }
}
