//! Issue #347: rift-lint honors NO_COLOR / non-TTY stdout and emits pure JSON with `-o json`.

use std::path::{Path, PathBuf};
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

/// Issue #1091: run the binary over a directory of single-imposter files, one per `(name, port)`.
fn lint_ports_dir(ports: &[(&str, &str)]) -> (serde_json::Value, Option<i32>) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, port) in ports {
        std::fs::write(
            dir.path().join(name),
            format!(r#"{{"port":{port},"protocol":"http","stubs":[]}}"#),
        )
        .expect("write");
    }
    let out = Command::new(BIN)
        .args([dir.path().to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let report = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}); exit {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (report, out.status.code())
}

fn issues_with_code<'a>(report: &'a serde_json::Value, code: &str) -> Vec<&'a serde_json::Value> {
    report["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter(|i| i["code"] == code)
        .collect()
}

#[test]
fn cli_reports_e002_once_for_two_files_on_one_port() {
    let (report, code) = lint_ports_dir(&[("a.json", "4545"), ("b.json", "4545")]);
    let e002 = issues_with_code(&report, "E002");
    assert_eq!(e002.len(), 1, "got {report}");
    assert_eq!(e002[0]["location"], "port");
    let message = e002[0]["message"].as_str().expect("message");
    assert!(
        message.starts_with("Port 4545 is used by 2 files:")
            && message.contains("a.json")
            && message.contains("b.json"),
        "got {message}"
    );
    assert_eq!(code, Some(1));
}

/// `70000 as u16` is 4464: the out-of-range port used to collide with a real one.
#[test]
fn cli_does_not_report_e002_for_a_wrapped_out_of_range_port() {
    let (report, code) = lint_ports_dir(&[("high.json", "70000"), ("real.json", "4464")]);
    assert_eq!(issues_with_code(&report, "E002").len(), 0, "got {report}");
    let e005 = issues_with_code(&report, "E005");
    assert_eq!(e005.len(), 1, "got {report}");
    assert!(
        e005[0]["file"]
            .as_str()
            .expect("file")
            .ends_with("high.json"),
        "got {report}"
    );
    assert_eq!(report["errors"], 1, "got {report}");
    assert_eq!(code, Some(1));
}

/// `65536 as u16` is 0, and port 0 is E005's too, so neither may reach the conflict map.
#[test]
fn cli_does_not_report_e002_between_two_out_of_range_ports() {
    let (report, _) = lint_ports_dir(&[("zero.json", "0"), ("wraps.json", "65536")]);
    assert_eq!(issues_with_code(&report, "E002").len(), 0, "got {report}");
    assert_eq!(issues_with_code(&report, "E005").len(), 2, "got {report}");

    let (report, _) = lint_ports_dir(&[("a.json", "0"), ("b.json", "0")]);
    assert_eq!(issues_with_code(&report, "E002").len(), 0, "got {report}");
    assert_eq!(issues_with_code(&report, "E005").len(), 2, "got {report}");
}

/// A conflict on the last port computed its suggestion as `port + 1`, which overflows a `u16`.
#[test]
fn cli_reports_e002_on_the_last_port_without_panicking() {
    let (report, code) = lint_ports_dir(&[("a.json", "65535"), ("b.json", "65535")]);
    let e002 = issues_with_code(&report, "E002");
    assert_eq!(e002.len(), 1, "got {report}");
    let suggestion = e002[0]["suggestion"].as_str().expect("suggestion");
    assert_eq!(suggestion, "Assign unique ports to each imposter");
    assert_eq!(code, Some(1), "a panic exits 101");
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

// ─── Issue #1076: `--fix` must not rewrite a file whose parse dropped a repeated key ───────────
//
// `--fix` rewrites the whole file from the collapsed `serde_json::Value`, so any byte-identical
// repeated key is already gone by the time it runs. Writing that back silently discards a value
// the author wrote — and in the `is.headers` case below, one the engine deliberately honours.

/// A numeric header is an E019 error, which both opens the `--fix` gate and is the thing `--fix`
/// repairs. Pins the baseline the refusals below are measured against.
#[test]
fn fix_quotes_a_numeric_header_and_rewrites_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    std::fs::write(
        &f,
        r#"{"port":3000,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256}}}]}
        ]}"#,
    )
    .expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    let after = std::fs::read_to_string(&f).expect("read back");
    assert!(
        after.contains(r#""Content-Length": "256""#),
        "the numeric header was quoted, got: {after}"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("Applied 1 fixes"), "got: {stdout}");
    assert!(
        !stdout.contains("skipped"),
        "with nothing skipped the summary keeps its old wording exactly, got: {stdout}"
    );
}

/// The worst case, and the reason this is a bug rather than a nit: a repeated key in `is.headers`
/// is how a stub sends two `Set-Cookie` lines. The engine merges it and the linter deliberately
/// reports nothing for it, so rewriting the file would halve the cookies with no finding anywhere.
#[test]
fn fix_refuses_a_file_whose_parse_dropped_a_deliberate_is_headers_duplicate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{
            "Content-Length":256,
            "Set-Cookie":"a=1",
            "Set-Cookie":"b=2"
        }}}]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(
        std::fs::read_to_string(&f).expect("read back"),
        original,
        "the file must be byte-identical; rewriting it would drop the first Set-Cookie"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("Skipped"),
        "the refusal is stated, got: {stdout}"
    );
    assert!(
        stdout.contains("Set-Cookie"),
        "it names the key, got: {stdout}"
    );
    assert!(
        stdout.contains("stubs[0].responses[0].is.headers"),
        "it names where, got: {stdout}"
    );
    assert!(
        stdout.contains("Applied 0 fixes"),
        "nothing was fixed in this file, got: {stdout}"
    );
}

/// The E044 case the issue described: a duplicate the linter *does* report. Same refusal.
#[test]
fn fix_refuses_a_file_carrying_e044() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[
            {"is":{"statusCode":200,"headers":{"Content-Length":256}}},
            {"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}
        ]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(
        std::fs::read_to_string(&f).expect("read back"),
        original,
        "a file carrying an E044 must not be rewritten"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("Skipped") && stdout.contains("X-Id"),
        "got: {stdout}"
    );
}

/// One bad file must not stop a good one from being repaired, and the summary must report both.
#[test]
fn fix_still_repairs_a_clean_file_when_another_is_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clean = dir.path().join("clean.json");
    let dup = dir.path().join("dup.json");
    std::fs::write(
        &clean,
        r#"{"port":3001,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256}}}]}
        ]}"#,
    )
    .expect("write");
    let dup_original = r#"{"port":3002,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{
            "Content-Length":512,"Set-Cookie":"a=1","Set-Cookie":"b=2"
        }}}]}
    ]}"#;
    std::fs::write(&dup, dup_original).expect("write");

    let out = Command::new(BIN)
        .args([dir.path().to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert!(
        std::fs::read_to_string(&clean)
            .expect("read back")
            .contains(r#""Content-Length": "256""#),
        "the clean file is still repaired"
    );
    assert_eq!(
        std::fs::read_to_string(&dup).expect("read back"),
        dup_original,
        "the duplicate-carrying file is left alone"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(stdout.contains("Applied 1 fixes"), "got: {stdout}");
    assert!(
        stdout.contains("skipped 1 file"),
        "the summary counts the skip, got: {stdout}"
    );
}

/// A duplicate with nothing fixable alongside it: the file was never going to be written, so there
/// is nothing at risk and no skip line to print.
#[test]
fn fix_says_nothing_about_a_duplicate_in_a_file_it_would_not_have_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(std::fs::read_to_string(&f).expect("read back"), original);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        !stdout.contains("Skipped"),
        "nothing was at risk, so no refusal is reported, got: {stdout}"
    );
}

/// A repeat in the document's root object has no path to name, so the message says so rather than
/// printing an empty location. Only reachable end-to-end, since the wording lives in `apply_fixes`.
#[test]
fn fix_names_the_document_root_when_the_duplicate_is_top_level() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"port":3001,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256}}}]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(std::fs::read_to_string(&f).expect("read back"), original);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("at the document root"),
        "a root-level repeat names the root, got: {stdout}"
    );
    assert!(stdout.contains("'port'"), "it names the key, got: {stdout}");
}

/// Several duplicates in one file are one refusal, not several: a line each so the author can fix
/// them all, but the file is counted once. A counter moved inside the inner loop would say
/// "skipped 2 files" for one file, and nothing else would catch that.
#[test]
fn several_duplicates_in_one_file_are_reported_each_but_counted_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[
            {"is":{"statusCode":200,"headers":{
                "Content-Length":256,"Set-Cookie":"a=1","Set-Cookie":"b=2"
            }}},
            {"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}
        ]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(std::fs::read_to_string(&f).expect("read back"), original);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.matches("Skipped:").count(),
        2,
        "one line per duplicate, got: {stdout}"
    );
    assert!(
        stdout.contains("skipped 1 file"),
        "one file, however many duplicates it holds, got: {stdout}"
    );
}

/// Issue #347's contract: with `-o json`, stdout is nothing but JSON. The refusal lines are new
/// output on that path, so they must go to stderr like every other `--fix` line.
#[test]
fn json_mode_keeps_refusals_off_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{
            "Content-Length":256,"Set-Cookie":"a=1","Set-Cookie":"b=2"
        }}}]}
    ]}"#;
    std::fs::write(&f, original).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix", "-o", "json"])
        .output()
        .expect("run rift-lint");

    assert_eq!(std::fs::read_to_string(&f).expect("read back"), original);
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    serde_json::from_str::<serde_json::Value>(stdout.trim()).expect("stdout is pure JSON");
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(
        stderr.contains("Skipped"),
        "the refusal is still reported, on stderr, got: {stderr}"
    );
}

/// A file that cannot be parsed never reaches the fixer, so it must not be reported as skipped —
/// and it must not stop a sibling file from being repaired.
#[test]
fn an_unparseable_file_is_not_reported_as_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let broken = dir.path().join("broken.json");
    let good = dir.path().join("good.json");
    std::fs::write(&broken, "{not json}").expect("write");
    std::fs::write(
        &good,
        r#"{"port":3003,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256}}}]}
        ]}"#,
    )
    .expect("write");

    let out = Command::new(BIN)
        .args([dir.path().to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(
        std::fs::read_to_string(&broken).expect("read back"),
        "{not json}",
        "the unparseable file is left alone"
    );
    assert!(
        std::fs::read_to_string(&good)
            .expect("read back")
            .contains(r#""Content-Length": "256""#),
        "its sibling is still repaired"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        !stdout.contains("Skipped"),
        "an unparseable file is an E001, not a refusal, got: {stdout}"
    );
}

// ─── Issue #1071: the CLI lints YAML, and `--fix` refuses to rewrite it ────────────────────────

const YAML_WITH_NUMERIC_HEADER: &str = "\
- port: 3000
  protocol: http
  stubs:
    - responses:
        - is:
            statusCode: 200
            headers:
              Content-Length: 256
";

/// The headline symptom: `rift-lint config.yaml` used to print "No JSON files found" and exit 0,
/// having checked nothing — on a file `--configfile` will happily load.
#[test]
fn cli_lints_a_yaml_file_instead_of_skipping_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("config.yaml");
    std::fs::write(&f, YAML_WITH_NUMERIC_HEADER).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    let codes: Vec<&str> = parsed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"E019"),
        "the YAML file was linted, got {codes:?}"
    );
    assert!(
        !out.status.success(),
        "an error-severity finding still exits non-zero"
    );
}

/// `--fix` re-serializes with `serde_json::to_string_pretty`. Applied to a `.yaml` file that would
/// write JSON text under a YAML name — and the engine would then silently reparse it as JSON,
/// because it begins with `[`. So `--fix` must refuse, and say so.
#[test]
fn fix_refuses_to_rewrite_a_yaml_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("config.yaml");
    std::fs::write(&f, YAML_WITH_NUMERIC_HEADER).expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(
        std::fs::read_to_string(&f).expect("read back"),
        YAML_WITH_NUMERIC_HEADER,
        "the YAML file must be byte-identical; --fix writes JSON"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    // "Skipped" alone is shared with the duplicate-key refusal, so a file skipped for the wrong
    // reason would satisfy it. The word YAML is what distinguishes this path.
    assert!(
        stdout.contains("Skipped") && stdout.contains("YAML"),
        "refused *because it is YAML*, got: {stdout}"
    );
}

/// A JSON file in the same run is still repaired, so the YAML refusal is not a blanket stop.
#[test]
fn fix_still_repairs_json_when_a_yaml_file_is_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let y = dir.path().join("config.yaml");
    let j = dir.path().join("imposter.json");
    std::fs::write(&y, YAML_WITH_NUMERIC_HEADER).expect("write");
    std::fs::write(
        &j,
        r#"{"port":3001,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256}}}]}
        ]}"#,
    )
    .expect("write");

    Command::new(BIN)
        .args([dir.path().to_str().unwrap(), "--fix"])
        .output()
        .expect("run rift-lint");

    assert_eq!(
        std::fs::read_to_string(&y).expect("read back"),
        YAML_WITH_NUMERIC_HEADER,
        "YAML untouched"
    );
    assert!(
        std::fs::read_to_string(&j)
            .expect("read back")
            .contains(r#""Content-Length": "256""#),
        "the JSON sibling is still repaired"
    );
}

/// The no-files warning must stop saying "JSON" now that YAML counts.
#[test]
fn the_no_files_warning_names_both_formats() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("readme.txt"), "hello").expect("write");

    let out = Command::new(BIN)
        .arg(dir.path().to_str().unwrap())
        .output()
        .expect("run rift-lint");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("YAML") && stdout.contains("JSON"),
        "the warning names both formats, not just the new one, got: {stdout}"
    );
}

/// Blocker this file exists to prevent: every other E046 test drives the library, and the CLI takes
/// a different route into it (`lint_document`). A rule raised on the library path only would be
/// invisible to almost every user — the same shape of gap #1069 was about.
#[test]
fn cli_reports_e046_for_a_yaml_mapping_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("config.yaml");
    std::fs::write(&f, "port: 3000\nprotocol: http\nstubs: []\n").expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    let codes: Vec<&str> = parsed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"E046"),
        "the binary reports it, got {codes:?}"
    );
    assert!(!out.status.success(), "and exits non-zero");
}

/// The engine sniffs the first non-whitespace byte, not the extension: `{` and `[` go to
/// `serde_json`. So a `.yaml` file holding a JSON document loads fine, and flagging it would be a
/// false positive on a working config.
#[test]
fn a_yaml_file_holding_json_is_not_e046() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in [
        (
            "wrapper.yaml",
            r#"{"imposters":[{"port":3000,"protocol":"http","stubs":[]}]}"#,
        ),
        (
            "single.yaml",
            r#"{"port":3001,"protocol":"http","stubs":[]}"#,
        ),
        (
            "array.yaml",
            r#"[{"port":3002,"protocol":"http","stubs":[]}]"#,
        ),
    ] {
        let f = dir.path().join(name);
        std::fs::write(&f, text).expect("write");

        let out = Command::new(BIN)
            .args([f.to_str().unwrap(), "-o", "json"])
            .output()
            .expect("run rift-lint");

        let stdout = String::from_utf8(out.stdout).expect("utf8");
        let parsed: serde_json::Value =
            serde_json::from_str(stdout.trim()).expect("stdout is JSON");
        let codes: Vec<&str> = parsed["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["code"].as_str())
            .collect();
        assert!(
            !codes.contains(&"E046"),
            "{name} is loaded by the engine's JSON branch, got {codes:?}"
        );
    }
}

// ─── Issue #1080: `--fix` must not rewrite a file whose number literal it cannot write back ────
//
// `--fix` re-serializes the whole file from the parsed `Value`, where a literal wider than `u64` or
// more precise than `f64` is already the nearest `f64`. Writing that back replaces digits the
// author wrote with different ones.

/// Runs `--fix` over one JSON file and returns (file after, stdout, stderr).
fn fix_one(original: &str, extra: &[&str]) -> (String, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    std::fs::write(&f, original).expect("write");
    let mut args = vec![f.to_str().unwrap(), "--fix"];
    args.extend_from_slice(extra);
    let out = Command::new(BIN)
        .args(&args)
        .output()
        .expect("run rift-lint");
    (
        std::fs::read_to_string(&f).expect("read back"),
        String::from_utf8(out.stdout).expect("utf8"),
        String::from_utf8(out.stderr).expect("utf8"),
    )
}

#[test]
fn fix_refuses_a_file_whose_integer_literal_would_be_rewritten() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256},
            "body":{"big":123456789012345678901234567890}}}]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert_eq!(
        after, original,
        "the file must be byte-identical; rewriting it would change the number"
    );
    assert!(stdout.contains("Skipped"), "got: {stdout}");
    assert!(
        stdout.contains("'123456789012345678901234567890' at line 3 column 27"),
        "it names the literal and where it is, got: {stdout}"
    );
    assert!(
        stdout.contains("1.2345678901234568e29"),
        "it says what the literal would become, got: {stdout}"
    );
    assert!(stdout.contains("Applied 0 fixes"), "got: {stdout}");
    assert!(
        !stdout.contains("Fixed header"),
        "a file that is not written fixed nothing, got: {stdout}"
    );
}

#[test]
fn fix_refuses_a_file_whose_decimal_literal_would_be_rewritten() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256},
            "body":{"precise":0.1000000000000000055511151231257827}}}]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert_eq!(after, original);
    assert!(
        stdout.contains("'0.1000000000000000055511151231257827'")
            && stdout.contains("would be written as 0.1,"),
        "got: {stdout}"
    );
}

/// The header `--fix` repairs is itself the literal it would corrupt: quoting it turns
/// `123456789012345678901234567890` into the string `"1.2345678901234568e29"`.
#[test]
fn fix_refuses_to_quote_a_header_whose_own_number_it_cannot_write_back() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"X-Trace":123456789012345678901234567890}}}]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert_eq!(after, original);
    assert!(
        stdout.contains("Skipped:") && stdout.contains("would be written as 1.2345678901234568e29"),
        "the refusal itself names the number (W012 also prints the literal), got: {stdout}"
    );
}

/// A number that only changes spelling is formatting, which the Auto-Fix docs already say is not
/// preserved. Refusing these would refuse ordinary files.
#[test]
fn fix_still_rewrites_a_file_whose_numbers_only_change_spelling() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256},
            "body":{"a":0.10,"b":1e2,"c":18446744073709551615,"d":-9223372036854775808}}}]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert!(stdout.contains("Applied 1 fixes"), "got: {stdout}");
    assert!(!stdout.contains("Skipped"), "got: {stdout}");
    for expected in [
        r#""a": 0.1"#,
        r#""b": 100.0"#,
        r#""c": 18446744073709551615"#,
        r#""d": -9223372036854775808"#,
        r#""Content-Length": "256""#,
    ] {
        assert!(after.contains(expected), "missing {expected} in: {after}");
    }
}

/// `--fix` only runs when the lint found an error, so the fixture carries one it cannot repair
/// (`E044`) — otherwise this would pass without ever reaching the check.
#[test]
fn fix_says_nothing_about_a_lossy_number_in_a_file_it_would_not_have_written() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[
            {"is":{"statusCode":200,"body":{"big":123456789012345678901234567890}}},
            {"proxy":{"to":"http://x","injectHeaders":{"X-Id":"a","X-Id":"b"}}}
        ]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert_eq!(after, original);
    assert!(
        stdout.contains("Applied 0 fixes"),
        "the fix pass ran, got: {stdout}"
    );
    assert!(
        !stdout.contains("Skipped"),
        "nothing was at risk, so no refusal is reported, got: {stdout}"
    );
}

#[test]
fn json_mode_keeps_number_refusals_off_stdout() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{"Content-Length":256},
            "body":{"big":123456789012345678901234567890}}}]}
    ]}"#;

    let (after, stdout, stderr) = fix_one(original, &["-o", "json"]);

    assert_eq!(after, original);
    serde_json::from_str::<serde_json::Value>(stdout.trim()).expect("stdout is pure JSON");
    assert!(
        stderr.contains("Skipped") && stderr.contains("123456789012345678901234567890"),
        "the refusal is still reported, on stderr, got: {stderr}"
    );
}

/// Both reasons are reported, so resolving one does not reveal the other on the next run; the file
/// is still counted once.
#[test]
fn a_file_with_a_repeated_key_and_a_lossy_number_reports_both_and_counts_once() {
    let original = r#"{"port":3000,"protocol":"http","stubs":[
        {"responses":[{"is":{"statusCode":200,"headers":{
            "Content-Length":256,"Set-Cookie":"a=1","Set-Cookie":"b=2"},
            "body":{"big":123456789012345678901234567890}}}]}
    ]}"#;

    let (after, stdout, _) = fix_one(original, &[]);

    assert_eq!(after, original);
    assert!(stdout.contains("Set-Cookie"), "got: {stdout}");
    assert!(
        stdout.contains("rewriting it would change a number: '123456789012345678901234567890'"),
        "the refusal itself names the number (W012 also prints the literal), got: {stdout}"
    );
    assert_eq!(stdout.matches("Skipped:").count(), 2, "got: {stdout}");
    assert!(stdout.contains("skipped 1 file "), "got: {stdout}");
}

// ─── Issue #1083: W012 through the binary ────────────────────────────────────────────────────────

/// The binary reaches W012 (it lints through `lint_document`), and a warning alone does not fail the
/// run — only `--strict` turns it into a failing exit.
#[test]
fn cli_reports_w012_as_a_warning_that_does_not_fail_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    std::fs::write(
        &f,
        r#"{"port":3000,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200,"body":{"big":123456789012345678901234567890}}}]}
        ]}"#,
    )
    .expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let w012: Vec<&serde_json::Value> = parsed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter(|i| i["code"] == "W012")
        .collect();
    assert_eq!(w012.len(), 1, "got {parsed}");
    assert_eq!(w012[0]["severity"], "warning");
    assert!(out.status.success(), "a warning alone exits 0");

    let strict = Command::new(BIN)
        .args([f.to_str().unwrap(), "--strict"])
        .output()
        .expect("run rift-lint");
    assert!(!strict.status.success(), "--strict fails on the warning");
}

/// Issue #1088, through the binary: emitting from a function the CLI never reaches has shipped
/// twice in this crate with every library test green.
fn e047_issues(path: &Path) -> (Vec<serde_json::Value>, bool) {
    let out = Command::new(BIN)
        .args([path.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let issues = parsed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter(|i| i["code"] == "E047")
        .cloned()
        .collect();
    (issues, out.status.success())
}

#[test]
fn cli_reports_e047_for_a_string_port() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    std::fs::write(&f, r#"{"port":"3000","protocol":"http","stubs":[]}"#).expect("write");

    let (issues, success) = e047_issues(&f);
    assert_eq!(issues.len(), 1, "got {issues:?}");
    assert_eq!(issues[0]["severity"], "error");
    assert_eq!(issues[0]["location"], "port");
    assert!(!success, "an error exits non-zero");
}

#[test]
fn cli_reports_e047_for_a_yaml_float_port() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("config.yaml");
    std::fs::write(&f, "- port: 3000.5\n  protocol: http\n  stubs: []\n").expect("write");

    let (issues, success) = e047_issues(&f);
    assert_eq!(issues.len(), 1, "got {issues:?}");
    assert_eq!(issues[0]["severity"], "error");
    assert_eq!(issues[0]["location"], "port");
    assert!(!success, "an error exits non-zero");
}

/// Issue #1093, through the binary.
#[test]
fn cli_is_clean_for_a_null_wait() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("imposter.json");
    std::fs::write(
        &f,
        r#"{"port":3000,"protocol":"http","stubs":[
            {"responses":[{"is":{"statusCode":200},"_behaviors":{"wait":null,"repeat":null}}]}
        ]}"#,
    )
    .expect("write");

    let out = Command::new(BIN)
        .args([f.to_str().unwrap(), "-o", "json"])
        .output()
        .expect("run rift-lint");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(report["issues"], serde_json::json!([]), "got {report}");
    assert!(out.status.success());
}
