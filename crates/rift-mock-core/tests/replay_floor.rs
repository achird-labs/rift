//! Issue #1268: the replay floor. `deserialize_replayed` (#1267) lets an embedder decode configs an
//! engine already admitted, without the admission checks. These tests fix what a replayed decode
//! still refuses (the stored format), pin `admission_check`'s contract, and fail when a new
//! decode-time refusal is added without being classified.

use rift_mock_core::imposter::{
    ImposterConfig, Stub, admission_check, admission_check_stub, deserialize_replayed,
};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

fn replayed<T: DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let value = deserialize_replayed(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------------------------------------
// The refusal census.
// ---------------------------------------------------------------------------------------------

/// The files a config decodes through: `ImposterConfig`, `Stub`, `StubResponse` and every type they
/// contain whose decode can refuse. `behaviors/types.rs` is absent on purpose: `ResponseBehaviors` is
/// only parsed inside the gated `refuse_unparseable_object`, an admission check.
const DECODE_FILES: &[&str] = &[
    "crates/rift-mock-core/src/imposter/types.rs",
    "crates/rift-types/src/wire.rs",
    "crates/rift-types/src/predicate.rs",
    "crates/rift-mock-core/src/behaviors/extraction.rs",
    "crates/rift-mock-core/src/behaviors/lookup.rs",
    "crates/rift-mock-core/src/extensions/state_ops.rs",
];

/// What can make a decode refuse: serde's error constructors, the attributes that route a field or a
/// type through code that can fail, hand-written impls, and the enum representations whose mismatch
/// is a refusal. `::custom` has no paren so `.map_err(serde::de::Error::custom)` counts (#1163's
/// spelling).
const MARKERS: &[(&str, &str)] = &[
    ("::custom", r"::custom\b"),
    (
        "serde error helper",
        r"\b(invalid_type|invalid_value|invalid_length|missing_field|unknown_variant|unknown_field|duplicate_field)\(",
    ),
    ("try_from attr", r#"\btry_from = ""#),
    ("deserialize_with attr", r#"\bdeserialize_with = ""#),
    ("deny_unknown_fields", r"\bdeny_unknown_fields\b"),
    ("untagged", r"\buntagged\b"),
    ("tag attr", r#"\btag = ""#),
    ("flatten", r"\bflatten\b"),
    ("impl Deserialize", r"impl<'de> [\w:]*Deserialize<'de> for"),
    ("impl Visitor", r"impl<'de> [\w:]*Visitor<'de> for"),
];

/// Functions whose body runs while a config decodes: `TryFrom` conversions, `deserialize_with`
/// helpers and visitors, and the `refuse_`/`validate_` checks they call. Their `?` operators and
/// `Err(` constructors are the refusals that no attribute announces.
const DECODE_FN: &str = r"\bfn (try_from|deserialize\w*|de_\w+|visit_\w+|refuse_\w+|validate_\w+|parse_status_code\w*)\b";

/// The counts, per file. When one changes, classify the new refusal before updating the number:
/// - an **admission check** (the decoded value still holds what it checks): run it only under
///   `Admission::Checked`, re-run it in `admission_check_stub`, and add a fixture to
///   `tests/issue_1267_replay_decode.rs`;
/// - **structural** (no engine ever admitted the input, in practice the shape rules of a field added
///   in the same change): leave it unconditional and add a fixture to
///   `the_floor_refusals_hold_on_replay`.
///
/// Anything else makes bytes an older engine admitted undecodable for an embedder replaying them
/// (rift-cluster#657). A change that only moves or rewords code may update the counts directly; the
/// failure message prints the whole current ledger.
const LEDGER: &[(&str, &str, usize)] = &[
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "::custom",
        13,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "serde error helper",
        2,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "try_from attr",
        3,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "deserialize_with attr",
        12,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "deny_unknown_fields",
        0,
    ),
    ("crates/rift-mock-core/src/imposter/types.rs", "untagged", 6),
    ("crates/rift-mock-core/src/imposter/types.rs", "tag attr", 1),
    ("crates/rift-mock-core/src/imposter/types.rs", "flatten", 2),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "impl Deserialize",
        1,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "impl Visitor",
        0,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "decode fn `?`",
        16,
    ),
    (
        "crates/rift-mock-core/src/imposter/types.rs",
        "decode fn `Err(`",
        14,
    ),
    ("crates/rift-types/src/wire.rs", "::custom", 4),
    ("crates/rift-types/src/wire.rs", "serde error helper", 0),
    ("crates/rift-types/src/wire.rs", "try_from attr", 0),
    ("crates/rift-types/src/wire.rs", "deserialize_with attr", 0),
    ("crates/rift-types/src/wire.rs", "deny_unknown_fields", 0),
    ("crates/rift-types/src/wire.rs", "untagged", 3),
    ("crates/rift-types/src/wire.rs", "tag attr", 0),
    ("crates/rift-types/src/wire.rs", "flatten", 0),
    ("crates/rift-types/src/wire.rs", "impl Deserialize", 0),
    ("crates/rift-types/src/wire.rs", "impl Visitor", 2),
    ("crates/rift-types/src/wire.rs", "decode fn `?`", 4),
    ("crates/rift-types/src/wire.rs", "decode fn `Err(`", 2),
    ("crates/rift-types/src/predicate.rs", "::custom", 0),
    (
        "crates/rift-types/src/predicate.rs",
        "serde error helper",
        0,
    ),
    ("crates/rift-types/src/predicate.rs", "try_from attr", 0),
    (
        "crates/rift-types/src/predicate.rs",
        "deserialize_with attr",
        0,
    ),
    (
        "crates/rift-types/src/predicate.rs",
        "deny_unknown_fields",
        0,
    ),
    ("crates/rift-types/src/predicate.rs", "untagged", 0),
    ("crates/rift-types/src/predicate.rs", "tag attr", 0),
    ("crates/rift-types/src/predicate.rs", "flatten", 3),
    ("crates/rift-types/src/predicate.rs", "impl Deserialize", 0),
    ("crates/rift-types/src/predicate.rs", "impl Visitor", 0),
    ("crates/rift-types/src/predicate.rs", "decode fn `?`", 0),
    ("crates/rift-types/src/predicate.rs", "decode fn `Err(`", 0),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "::custom",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "serde error helper",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "try_from attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "deserialize_with attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "deny_unknown_fields",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "untagged",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "tag attr",
        1,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "flatten",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "impl Deserialize",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "impl Visitor",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "decode fn `?`",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/extraction.rs",
        "decode fn `Err(`",
        3,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "::custom",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "serde error helper",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "try_from attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "deserialize_with attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "deny_unknown_fields",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "untagged",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "tag attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "flatten",
        3,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "impl Deserialize",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "impl Visitor",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "decode fn `?`",
        0,
    ),
    (
        "crates/rift-mock-core/src/behaviors/lookup.rs",
        "decode fn `Err(`",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "::custom",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "serde error helper",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "try_from attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "deserialize_with attr",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "deny_unknown_fields",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "untagged",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "tag attr",
        1,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "flatten",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "impl Deserialize",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "impl Visitor",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "decode fn `?`",
        0,
    ),
    (
        "crates/rift-mock-core/src/extensions/state_ops.rs",
        "decode fn `Err(`",
        0,
    ),
];

/// The file without its trailing unit-test module, whose fixtures are not decode paths.
fn production_source(relative: &str) -> String {
    let text = std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"));
    assert!(
        text.len() > 500,
        "{relative} looks empty: the census would be vacuous"
    );
    match text.find("\n#[cfg(test)]\nmod tests") {
        Some(end) => text[..end].to_string(),
        None => text,
    }
}

/// The brace-balanced body of the item whose header starts at `start`.
fn block_at(text: &str, start: usize) -> &str {
    let open = start + text[start..].find('{').expect("item has a body");
    let mut depth = 0usize;
    for (offset, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced item body");
}

fn census(relative: &str) -> Vec<(String, usize)> {
    let source = production_source(relative);
    let mut counts: Vec<(String, usize)> = MARKERS
        .iter()
        .map(|(name, pattern)| {
            let found = regex::Regex::new(pattern)
                .expect("marker pattern is a valid constant regex")
                .find_iter(&source)
                .count();
            ((*name).to_string(), found)
        })
        .collect();
    let decode_fn = regex::Regex::new(DECODE_FN).expect("valid constant regex");
    let (mut questions, mut errs) = (0, 0);
    for found in decode_fn.find_iter(&source) {
        let body = block_at(&source, found.start());
        questions += body.matches('?').count() - body.matches("?Sized").count();
        errs += body.matches("Err(").count();
    }
    counts.push(("decode fn `?`".to_string(), questions));
    counts.push(("decode fn `Err(`".to_string(), errs));
    counts
}

#[test]
fn the_decode_layer_has_no_unclassified_refusal() {
    let mut drift = Vec::new();
    let mut current = Vec::new();
    for file in DECODE_FILES {
        for (marker, found) in census(file) {
            current.push(format!("    (\"{file}\", \"{marker}\", {found}),"));
            let pinned = LEDGER
                .iter()
                .find(|(f, m, _)| f == file && *m == marker)
                .map(|(_, _, n)| *n);
            if pinned != Some(found) {
                drift.push(format!("  {file}: {marker} {found}, ledger {pinned:?}"));
            }
        }
    }
    assert!(
        drift.is_empty(),
        "the decode layer changed (issue #1268). A refusal added while a config decodes makes bytes \
         an older engine admitted undecodable for an embedder replaying them. Classify it as \
         described on `LEDGER`, then update the counts.\n{}\n\nCurrent ledger:\n{}",
        drift.join("\n"),
        current.join("\n")
    );
    let nonzero = LEDGER.iter().filter(|(_, _, n)| *n > 0).count();
    assert!(
        nonzero >= 10,
        "the census found almost nothing: the scanner is broken, not the decode layer clean"
    );
}

// ---------------------------------------------------------------------------------------------
// The floor: what a replayed decode still refuses.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_floor_refusals_hold_on_replay() {
    let cases = [
        (
            r#"{"responses":[{"is":{"statusCode":200},"_behaviors":[{"wait":5}]}]}"#,
            "`_behaviors` to be an object (the array form is spelled `behaviors`)",
        ),
        (
            r#"{"responses":[{"is":{"statusCode":200},"behaviors":5}]}"#,
            "`behaviors` to be an object or an array of behavior objects",
        ),
        (
            r#"{"responses":[{"is":{"statusCode":200},"_behaviors":{"wait":{"min":5,"max":1}}}]}"#,
            "`wait` range has min 5 greater than max 1",
        ),
        (
            r#"{"responses":[{"is":{"statusCode":200},"behaviors":[{"wait":{"min":5,"max":1}},{"wait":null}]}]}"#,
            "`wait` range has min 5 greater than max 1",
        ),
        (
            r#"{"delayRange":[{"min":5,"max":1}],"responses":[{"is":{"statusCode":200}}]}"#,
            "`delayRange` has min 5 greater than max 1",
        ),
        (
            r#"{"responses":[{"proxy":{"to":"http://x","injectHeaders":{"X-A":"1","x-a":"2"}}}]}"#,
            "header `x-a` is already given as `X-A`",
        ),
        (
            r#"{"responses":[{"is":{"statusCode":200},"_rift":{"fault":{"tcp":{"probability":1.5,"type":"CONNECTION_RESET_BY_PEER"}}}}]}"#,
            "_rift.fault.tcp 'probability' must be between 0.0 and 1.0",
        ),
        (
            r#"{"responses":[{"is":{"statusCode":200},"_rift":{"fault":{"tcp":{"probability":0.5}}}}]}"#,
            "_rift.fault.tcp object form requires a string 'type'",
        ),
    ];
    for (stub, refusal) in cases {
        let door = serde_json::from_str::<Stub>(stub)
            .expect_err("the door refuses")
            .to_string();
        let replay = replayed::<Stub>(stub)
            .expect_err("replay refuses too: this is part of the stored format")
            .to_string();
        assert!(door.contains(refusal), "door: {door}");
        assert_eq!(replay, door, "replay must refuse with the door's message");
    }
}

// ---------------------------------------------------------------------------------------------
// Door admits ⇒ replay admits, to an equal value, and admission_check passes.
// ---------------------------------------------------------------------------------------------

fn imposters_in(path: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).expect("read config");
    let value: serde_json::Value = serde_json::from_str(&text).expect("config is JSON");
    match value.get("imposters").and_then(serde_json::Value::as_array) {
        Some(imposters) => imposters.clone(),
        None => vec![value],
    }
}

#[test]
fn a_door_admitted_config_replays_to_the_same_value() {
    let root = repo_root();
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in ["sdk-conformance/corpus/imposters", "docs/demo"] {
        for entry in std::fs::read_dir(root.join(dir)).expect("read config dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut seen = 0;
    for path in &files {
        for imposter in imposters_in(path) {
            let text = imposter.to_string();
            let door: ImposterConfig = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: the door refuses: {e}", path.display()));
            let replay: ImposterConfig = replayed(&text)
                .unwrap_or_else(|e| panic!("{}: replay refuses: {e}", path.display()));
            assert_eq!(
                serde_json::to_value(&replay).expect("serialize replayed"),
                serde_json::to_value(&door).expect("serialize door"),
                "{}",
                path.display()
            );
            assert_eq!(admission_check(&replay), Ok(()), "{}", path.display());
            seen += 1;
        }
    }
    assert!(
        seen >= 20,
        "the corpora must not be silently empty: {seen} imposters"
    );
}

// ---------------------------------------------------------------------------------------------
// admission_check's contract.
// ---------------------------------------------------------------------------------------------

/// Responses decode before the stub's predicates are checked, so the door names the `copy`
/// selector; `admission_check_stub` must report the same first failure.
#[test]
fn admission_check_reports_the_first_failure_in_decode_order() {
    let stub = r#"{
        "predicates": [{ "equals": { "body": "x" }, "jsonpath": { "selector": "$[[[bad" } }],
        "responses": [{
            "is": { "statusCode": 200 },
            "_behaviors": { "copy": { "from": "body", "into": "${T}",
                                       "using": { "method": "regex", "selector": "(unclosed" } } }
        }]
    }"#;
    let first = "`copy` behavior `regex` selector `(unclosed` is invalid";
    let door = serde_json::from_str::<Stub>(stub)
        .expect_err("door refuses")
        .to_string();
    assert!(door.contains(first), "door: {door}");
    let decoded: Stub = replayed(stub).expect("replay admits");
    let checked = admission_check_stub(&decoded).expect_err("admission_check_stub refuses");
    assert!(
        checked.starts_with(first),
        "admission_check_stub: {checked}"
    );
}

/// The door checks a behaviors block before the top-level `repeat`, but the decoded program holds
/// that `repeat` first; when both are malformed, the door names the block step and
/// `admission_check_stub` names the `repeat`. Both refuse.
#[test]
fn a_malformed_top_level_repeat_is_reported_before_the_block() {
    let stub = r#"{"responses":[{"is":{"statusCode":200},"repeat":2.5,
        "_behaviors":{"copy":{"from":"body","into":"${T}","using":{"method":"regex","selector":"(unclosed"}}}}]}"#;
    let door = serde_json::from_str::<Stub>(stub)
        .expect_err("door refuses")
        .to_string();
    assert!(
        door.contains("`copy` behavior `regex` selector `(unclosed` is invalid"),
        "door: {door}"
    );
    let decoded: Stub = replayed(stub).expect("replay admits");
    let checked = admission_check_stub(&decoded).expect_err("admission_check_stub refuses");
    assert!(
        checked.starts_with("`repeat` behavior is malformed"),
        "admission_check_stub: {checked}"
    );
}

/// The door checks a behaviors block before a top-level `repeat` replaces the block's `repeat`;
/// the decoded program no longer holds the replaced step, so only the door refuses it. Harmless at
/// serve time: the program that runs is `repeat: 3`.
#[test]
fn a_replaced_block_repeat_is_the_doors_refusal_alone() {
    let stub =
        r#"{"responses":[{"is":{"statusCode":200},"_behaviors":{"repeat":2.5},"repeat":3}]}"#;
    let door = serde_json::from_str::<Stub>(stub)
        .expect_err("door refuses")
        .to_string();
    assert!(
        door.contains("`repeat` behavior is malformed"),
        "door: {door}"
    );
    let decoded: Stub = replayed(stub).expect("replay admits");
    assert_eq!(admission_check_stub(&decoded), Ok(()));
}
