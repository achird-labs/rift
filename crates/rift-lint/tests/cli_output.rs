//! Issue #347: rift-lint honors NO_COLOR / non-TTY stdout and emits pure JSON with `-o json`.

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-lint");

fn write_tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "rift_lint_347_{}_{}.json",
        std::process::id(),
        line!()
    ));
    std::fs::write(&p, r#"{"port":8000,"protocol":"http","stubs":[]}"#).expect("write");
    p
}

// AC3: `-o json` prints ONLY JSON on stdout — no decorative banner, no ANSI escapes.
#[test]
fn lint_json_stdout_is_pure_json() {
    let f = write_tmp();
    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        !stdout.contains('\x1b'),
        "json-mode stdout must contain no ANSI escapes, got: {stdout:?}"
    );
    assert!(
        !stdout.contains("Rift Imposter Linter"),
        "json-mode stdout must not carry the human banner"
    );
    serde_json::from_str::<serde_json::Value>(stdout.trim()).expect("stdout parses as JSON");
    let _ = std::fs::remove_file(f);
}

// AC1: text mode with a piped (non-TTY) stdout — as in this test — emits no ANSI and no banner.
#[test]
fn lint_text_piped_has_no_ansi_or_banner() {
    let f = write_tmp();
    let out = Command::new(BIN)
        .arg(f.to_str().unwrap())
        .output()
        .expect("run rift-lint");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        !stdout.contains('\x1b'),
        "piped stdout must contain no ANSI escapes, got: {stdout:?}"
    );
    assert!(
        !stdout.contains("Rift Imposter Linter"),
        "piped stdout must not carry the decorative banner"
    );
    let _ = std::fs::remove_file(f);
}

// AC3 edge: `-o json` on a directory with no imposter files still emits valid JSON on stdout
// (not empty input), so a consumer piping to `jq` never chokes.
#[test]
fn lint_json_empty_dir_still_emits_json() {
    let dir = std::env::temp_dir().join(format!("rift_lint_347_empty_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let out = Command::new(BIN)
        .args([dir.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .expect("no-files json mode still yields valid JSON on stdout");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lint_json_directory_attributes_parse_and_validation_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let valid_json = dir.path().join("valid-json.json");
    let invalid_json = dir.path().join("invalid-json.json");
    // Valid JSON that intentionally reaches pass-two validation with one missing field.
    std::fs::write(&valid_json, r#"{"protocol":"http","stubs":[]}"#).expect("write valid JSON");
    std::fs::write(&invalid_json, r#"{"port":8001"#).expect("write invalid JSON");

    let out = Command::new(BIN)
        .args([dir.path().to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    assert!(
        !out.status.success(),
        "lint errors must produce a failure exit"
    );

    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout parses as JSON");
    let issues = report["issues"].as_array().expect("issues is an array");
    assert_eq!(report["files_checked"].as_u64(), Some(2));
    assert_eq!(report["errors"].as_u64(), Some(2));
    assert_eq!(issues.len(), 2, "expected one issue per file: {issues:?}");

    let parse_errors: Vec<_> = issues
        .iter()
        .filter(|issue| {
            issue["code"] == "E001"
                && issue["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with("Failed to parse JSON:"))
        })
        .collect();
    assert_eq!(
        parse_errors.len(),
        1,
        "expected one parse error: {issues:?}"
    );
    assert_eq!(
        parse_errors[0]["file"],
        invalid_json.to_string_lossy().as_ref()
    );

    let validation_issues: Vec<_> = issues
        .iter()
        .filter(|issue| issue["code"] == "E003")
        .collect();
    assert_eq!(
        validation_issues.len(),
        1,
        "valid JSON should produce one validation issue: {issues:?}"
    );
    assert_eq!(
        validation_issues[0]["file"],
        valid_json.to_string_lossy().as_ref()
    );
}

// AC1: NO_COLOR is honored regardless of TTY.
#[test]
fn lint_no_color_env_disables_ansi() {
    let f = write_tmp();
    let out = Command::new(BIN)
        .arg(f.to_str().unwrap())
        .env("NO_COLOR", "1")
        .output()
        .expect("run rift-lint");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        !stdout.contains('\x1b'),
        "NO_COLOR must disable ANSI escapes, got: {stdout:?}"
    );
    let _ = std::fs::remove_file(f);
}

// Issue #1069: the binary is the path that bypassed the raw text entirely — it parsed to a `Value`
// in `load_imposter_file` and linted that, so no rule reading the document text could ever fire for
// someone actually running `rift-lint`. This is the regression that matters.
#[test]
fn cli_reports_a_byte_identical_duplicate_key() {
    let f = std::env::temp_dir().join(format!(
        "rift_lint_1069_{}_{}.json",
        std::process::id(),
        line!()
    ));
    std::fs::write(
        &f,
        r#"{"port":8001,"protocol":"http","stubs":[
            {"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}]}
        ]}"#,
    )
    .expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    let codes: Vec<&str> = parsed["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"E044"),
        "the binary must report the duplicate it used to be blind to, got {codes:?}"
    );
    assert!(
        !out.status.success(),
        "an error-severity finding must make the binary exit non-zero"
    );
    let _ = std::fs::remove_file(f);
}
