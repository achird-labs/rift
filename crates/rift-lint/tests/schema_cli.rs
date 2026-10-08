//! `rift-lint schema` (issue #1342), through the binary: what it prints is the checked-in
//! schema, `--out` writes the same bytes, and linting still works beside it. Empty without the
//! `schema` feature, which is also how the released binary is built.
#![cfg(feature = "schema")]

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-lint");

fn checked_in() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk-conformance/schema/imposter.schema.json");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rift_lint_1342_{}_{name}", std::process::id()))
}

/// Run the binary with `args`, asserting it succeeded; the caller inspects its output.
fn run_ok(args: &[&std::ffi::OsStr]) -> std::process::Output {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("run rift-lint");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn schema_prints_the_checked_in_schema() {
    let out = run_ok(&["schema".as_ref()]);
    assert_eq!(String::from_utf8(out.stdout).expect("utf8"), checked_in());
    assert!(out.stderr.is_empty(), "no banner on stderr for `schema`");
}

#[test]
fn schema_out_writes_the_same_bytes() {
    let file = tmp("out.json");
    let out = run_ok(&["schema".as_ref(), "--out".as_ref(), file.as_os_str()]);
    assert!(
        out.stdout.is_empty(),
        "nothing on stdout when writing a file"
    );
    let written = std::fs::read_to_string(&file).expect("the file was written");
    let _ = std::fs::remove_file(&file);
    assert_eq!(written, checked_in());
}

#[test]
fn schema_out_to_an_unwritable_path_fails_loudly() {
    let dir = tmp("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let out = Command::new(BIN)
        .args(["schema", "--out"])
        .arg(&dir)
        .output()
        .expect("run rift-lint schema --out <dir>");
    let _ = std::fs::remove_dir(&dir);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot write"), "{stderr}");
}

#[test]
fn linting_still_takes_a_path_beside_the_subcommand() {
    let file = tmp("imposter.json");
    std::fs::write(&file, r#"{"port":8000,"protocol":"http","stubs":[]}"#).expect("write");
    let out = run_ok(&[file.as_os_str(), "-o".as_ref(), "json".as_ref()]);
    let _ = std::fs::remove_file(&file);
    serde_json::from_str::<serde_json::Value>(String::from_utf8_lossy(&out.stdout).trim())
        .expect("lint result on stdout");

    let out = Command::new(BIN).output().expect("run rift-lint");
    assert!(
        !out.status.success(),
        "a path is still required without a subcommand"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("<PATH>"));
}
