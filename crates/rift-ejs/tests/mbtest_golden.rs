//! Issue #1604: mbTest's own `--configfile` fixture trees render to the same JSON Mountebank's
//! renderer produces for them.
//!
//! The trees under `tests/fixtures/mbtest/` are copied unchanged from Mountebank v2.9.5's
//! `mbTest/cli/` (MIT, © mountebank; see `tests/fixtures/mbtest/README.md`). Each `expected.json`
//! is the output of Mountebank's formatter (`@mbtest/mountebank-formatters` 0.0.2, EJS 2.7.4) for
//! that tree's `imposters.ejs`, generated once with
//!
//! ```sh
//! cd crates/rift-ejs/tests/fixtures/mbtest && node generate.js ~/bench-mb/node_modules
//! ```

use rift_ejs::{FileAccess, render};
use std::path::Path;

fn assert_renders_like_mountebank(tree: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mbtest")
        .join(tree);
    let root = dir.join("imposters.ejs");
    let content = std::fs::read_to_string(&root).expect("read imposters.ejs");
    let rendered = match render(&content, &root, FileAccess::Allowed) {
        Ok(rendered) => rendered,
        Err(e) => panic!("{tree}: {e}"),
    };
    assert!(
        rendered.unset_env.is_empty(),
        "{tree}: {:?}",
        rendered.unset_env
    );
    let ours: serde_json::Value = match serde_json::from_str(&rendered.text) {
        Ok(value) => value,
        Err(e) => panic!(
            "{tree}: the rendered text is not JSON ({e}):\n{}",
            rendered.text
        ),
    };
    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("expected.json")).expect("read expected.json"),
    )
    .expect("expected.json is JSON");
    assert_eq!(ours, expected, "{tree}");
}

#[test]
fn imposters_renders_like_mountebank() {
    assert_renders_like_mountebank("imposters");
}

#[test]
fn templates_renders_like_mountebank() {
    assert_renders_like_mountebank("templates");
}

#[test]
fn nested_stringify_renders_like_mountebank() {
    assert_renders_like_mountebank("nestedStringify");
}

#[test]
fn data_stringify_renders_like_mountebank() {
    assert_renders_like_mountebank("dataStringify");
}

#[test]
fn legacy_include_renders_like_mountebank() {
    assert_renders_like_mountebank("legacyInclude");
}
