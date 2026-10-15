//! `rift-lint` and `rift-verify` as subprocesses (port of `RiftTools.scala`). They are the binaries
//! staged beside the engine, so lint/verify verdicts always come from the build under test.

use crate::HarnessError;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// The flags the corpus gate verifies with. `--verbose` puts per-test detail on stderr; the JSON
/// summary on stdout is unaffected (rift#347).
pub const CORPUS_VERIFY_ARGS: &[&str] = &["--skip-dynamic", "--verify-dynamic", "--verbose"];

#[derive(Debug, Clone)]
pub struct Tools {
    pub engine: PathBuf,
    pub lint: PathBuf,
    pub verify: PathBuf,
}

/// One finding from `rift-lint -o json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LintIssue {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub file: String,
    pub location: Option<String>,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LintReport {
    pub errors: usize,
    pub warnings: usize,
    pub files_checked: usize,
    pub issues: Vec<LintIssue>,
    pub exit_code: i32,
    pub raw: String,
}

impl LintReport {
    /// The error-severity findings as `<file> <code>[@location]`, deduplicated: what a failing gate
    /// names so the offending file is in the message.
    #[must_use]
    pub fn error_offenders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for issue in self.issues.iter().filter(|i| i.severity == "error") {
            let file = issue.file.rsplit('/').next().unwrap_or(&issue.file);
            let at = issue
                .location
                .as_deref()
                .map_or_else(String::new, |l| format!("@{l}"));
            let line = format!("{file} {}{at}", issue.code);
            if !out.contains(&line) {
                out.push(line);
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub exit_code: i32,
    pub raw: String,
    /// The per-test `FAIL` rows rift-verify writes to stderr, each with its indented reason.
    pub fail_detail: Vec<String>,
    pub raw_err: String,
}

/// Captured output of one tool run.
#[derive(Debug, Clone)]
pub struct RawRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Deserialize)]
struct LintJson {
    issues: Vec<LintIssue>,
    files_checked: usize,
    errors: usize,
    warnings: usize,
}

/// `rift-verify -o json` emits more fields; only the tallies are needed.
#[derive(Deserialize)]
struct VerifyJson {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Tools {
    #[must_use]
    pub fn from_build_dir(build_dir: &Path) -> Self {
        Self {
            engine: build_dir.join("rift"),
            lint: build_dir.join("rift-lint"),
            verify: build_dir.join("rift-verify"),
        }
    }

    /// `rift-lint -o json <path>`.
    pub async fn lint(&self, path: &Path) -> Result<LintReport, HarnessError> {
        let run = run_capture(
            "rift-lint",
            Command::new(&self.lint).arg("-o").arg("json").arg(path),
        )
        .await?;
        parse_lint(run)
    }

    /// `rift-verify --admin-url <url> <args> -o json` against a running engine.
    pub async fn verify(
        &self,
        admin_url: &str,
        args: &[&str],
    ) -> Result<VerifyReport, HarnessError> {
        let run = run_capture(
            "rift-verify",
            Command::new(&self.verify)
                .arg("--admin-url")
                .arg(admin_url)
                .args(args)
                .arg("-o")
                .arg("json"),
        )
        .await?;
        parse_verify(run)
    }
}

/// First non-empty line of `<binary> --version` on stdout, the same thing the staging step
/// recorded into `BUILD_INFO`.
pub async fn binary_version(binary: &Path) -> Result<String, HarnessError> {
    let run = run_capture("--version", Command::new(binary).arg("--version")).await?;
    Ok(run
        .stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string())
}

/// Run to completion, capturing stdout and stderr, without failing on a non-zero exit: the tools
/// report findings through their exit code and the report is wanted either way.
async fn run_capture(tool: &'static str, command: &mut Command) -> Result<RawRun, HarnessError> {
    let program = PathBuf::from(command.as_std().get_program());
    let out = command
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|source| HarnessError::Io {
            action: "running",
            path: program,
            source,
        })?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let Some(exit_code) = out.status.code() else {
        return Err(HarnessError::ToolKilled { tool, stderr });
    };
    Ok(RawRun {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
        exit_code,
    })
}

/// Parse `rift-lint -o json` (since rift#347 stdout is the JSON alone).
pub fn parse_lint(run: RawRun) -> Result<LintReport, HarnessError> {
    match serde_json::from_str::<LintJson>(&run.stdout) {
        Ok(j) => Ok(LintReport {
            errors: j.errors,
            warnings: j.warnings,
            files_checked: j.files_checked,
            issues: j.issues,
            exit_code: run.exit_code,
            raw: run.stdout,
        }),
        Err(source) => Err(HarnessError::ToolOutput {
            tool: "rift-lint",
            exit: run.exit_code,
            stdout: run.stdout,
            stderr: run.stderr,
            source,
        }),
    }
}

/// Parse the `rift-verify -o json` summary, keeping the stderr `FAIL` detail.
pub fn parse_verify(run: RawRun) -> Result<VerifyReport, HarnessError> {
    match serde_json::from_str::<VerifyJson>(&run.stdout) {
        Ok(j) => Ok(VerifyReport {
            passed: j.passed,
            failed: j.failed,
            skipped: j.skipped,
            exit_code: run.exit_code,
            fail_detail: fail_lines(&run.stderr),
            raw: run.stdout,
            raw_err: run.stderr,
        }),
        Err(source) => Err(HarnessError::ToolOutput {
            tool: "rift-verify",
            exit: run.exit_code,
            stdout: run.stdout,
            stderr: run.stderr,
            source,
        }),
    }
}

const VERDICTS: &[&str] = &["FAIL ", "PASS ", "SKIP ", "Failed:", "Passed:", "Skipped:"];

/// The `FAIL` rows plus the indented reason lines after each. A continuation is an indented line
/// that is not itself a verdict or summary row, so the next stub's `PASS` row never becomes part of
/// this one's reason.
fn fail_lines(stderr: &str) -> Vec<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    let is_verdict_row = |l: &str| {
        let t = l.trim();
        VERDICTS.iter().any(|v| t.starts_with(v)) || t.starts_with("Imposter:")
    };
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().starts_with("FAIL "))
        .map(|(i, l)| {
            let mut parts = vec![l.trim()];
            parts.extend(
                lines[i + 1..]
                    .iter()
                    .take_while(|n| n.starts_with(' ') && !is_verdict_row(n))
                    .map(|n| n.trim()),
            );
            parts.join(" — ")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stdout: &str, stderr: &str, exit_code: i32) -> RawRun {
        RawRun {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    #[test]
    fn lint_json_parses_and_names_error_offenders_once() {
        let report = parse_lint(run(
            r#"{"issues":[
                {"severity":"error","code":"E001","message":"m","file":"/c/imposters/01.json","location":"stubs[0]","suggestion":null},
                {"severity":"error","code":"E001","message":"m","file":"/c/imposters/01.json","location":"stubs[0]","suggestion":null},
                {"severity":"warning","code":"W001","message":"m","file":"/c/imposters/02.json","location":null,"suggestion":null}],
              "files_checked":2,"errors":2,"warnings":1}"#,
            "",
            1,
        ))
        .expect("valid lint JSON");
        assert_eq!(report.files_checked, 2);
        assert_eq!(report.error_offenders(), vec!["01.json E001@stubs[0]"]);
    }

    #[test]
    fn unparseable_lint_output_is_an_error_carrying_stderr() {
        let err =
            parse_lint(run("not json", "thread 'main' panicked", 101)).expect_err("must not parse");
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[test]
    fn verify_keeps_fail_rows_with_their_reasons_and_not_the_next_pass_row() {
        let report = parse_verify(run(
            r#"{"passed":1,"failed":1,"skipped":0,"imposters":1}"#,
            "Imposter: 4501\n  FAIL GET /a\n    expected 200\n    got 404\n  PASS GET /b\nFailed: 1\n",
            1,
        ))
        .expect("valid verify JSON");
        assert_eq!(report.failed, 1);
        assert_eq!(
            report.fail_detail,
            vec!["FAIL GET /a — expected 200 — got 404"]
        );
    }
}
