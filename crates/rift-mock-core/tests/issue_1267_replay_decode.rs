//! Issue #1267: an embedder replaying configs the engine already admitted (rift-cluster's log and
//! tables) must be able to decode them without the admission checks the config doors run, and run
//! those checks separately. Decoding stored bytes must not depend on which admission rules the
//! current engine has: #1262 refused a `copy` selector #1258's predecessor admitted, and a node
//! holding that entry stopped starting.

use rift_mock_core::imposter::{
    ImposterConfig, Stub, admission_check, admission_check_stub, deserialize_replayed,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

const BAD_JSONPATH: &str = r#"{
    "predicates": [{ "equals": { "body": "x" }, "jsonpath": { "selector": "$[[[bad" } }],
    "responses": [{ "is": { "statusCode": 200 } }]
}"#;

const BAD_MATCHES: &str = r#"{
    "predicates": [{ "matches": { "path": "(unclosed" } }],
    "responses": [{ "is": { "statusCode": 200 } }]
}"#;

const BAD_COPY: &str = r#"{
    "responses": [{
        "is": { "statusCode": 200, "body": "${T}" },
        "_behaviors": { "copy": { "from": "body", "into": "${T}",
                                   "using": { "method": "jsonpath", "selector": "$[[[bad" } } }
    }]
}"#;

const GOOD: &str = r#"{
    "predicates": [{ "equals": { "path": "/ok" } }],
    "responses": [{ "is": { "statusCode": 200, "body": "ok" } }]
}"#;

/// A behaviors block that does not parse (#1162): stored as written, so it is an admission check.
const BAD_BLOCK: &str = r#"{
    "responses": [{ "is": { "statusCode": 200 }, "_behaviors": { "repeat": 2.5 } }]
}"#;

/// The same bad `copy` selector on a `proxy` and on a `fault` response: the door checks the block
/// before the response type is chosen, and so must `admission_check_stub`.
const BAD_COPY_ON_PROXY: &str = r#"{
    "responses": [{
        "proxy": { "to": "http://127.0.0.1:9" },
        "behaviors": { "copy": { "from": "body", "into": "${T}",
                                 "using": { "method": "jsonpath", "selector": "$[[[bad" } } }
    }]
}"#;

/// A fixed `_rift.conditional.lastModified` that is not an HTTP-date (issue #1280), on an `is`
/// response and on a `proxy` response, where the block is ignored but still checked.
const BAD_CONDITIONAL_DATE: &str = r#"{
    "responses": [{ "is": { "statusCode": 200 },
                    "_rift": { "conditional": { "lastModified": "Saturday" } } }]
}"#;

const BAD_CONDITIONAL_DATE_ON_PROXY: &str = r#"{
    "responses": [{ "proxy": { "to": "http://127.0.0.1:9" },
                    "_rift": { "conditional": { "etag": false, "lastModified": "2026-10-03" } } }]
}"#;

const BAD_PROXY_MODE: &str = r#"{
    "responses": [{ "proxy": { "to": "http://127.0.0.1:9", "mode": "bogus" } }]
}"#;

const BAD_COPY_ON_FAULT: &str = r#"{
    "responses": [{
        "fault": "CONNECTION_RESET_BY_PEER",
        "_behaviors": { "copy": { "from": "body", "into": "${T}",
                                   "using": { "method": "jsonpath", "selector": "$[[[bad" } } }
    }]
}"#;

fn replayed<T: DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let value = deserialize_replayed(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn imposter_with(stub: &str) -> String {
    format!(r#"{{ "port": 4700, "protocol": "http", "stubs": [{stub}] }}"#)
}

#[test]
fn each_admission_refusal_is_skipped_on_replay_and_reported_by_admission_check() {
    let cases = [
        (
            BAD_JSONPATH,
            "predicate `jsonpath` selector `$[[[bad` is invalid",
        ),
        (
            BAD_MATCHES,
            "predicate `matches` field `path` has an invalid regex `(unclosed`",
        ),
        (
            BAD_COPY,
            "`copy` behavior `jsonpath` selector `$[[[bad` is invalid",
        ),
        (BAD_BLOCK, "`repeat` behavior is malformed"),
        (
            BAD_COPY_ON_PROXY,
            "`copy` behavior `jsonpath` selector `$[[[bad` is invalid",
        ),
        (
            BAD_COPY_ON_FAULT,
            "`copy` behavior `jsonpath` selector `$[[[bad` is invalid",
        ),
        (
            BAD_CONDITIONAL_DATE,
            "`_rift.conditional.lastModified` must be \"load\" or an HTTP-date",
        ),
        (
            BAD_CONDITIONAL_DATE_ON_PROXY,
            "`_rift.conditional.lastModified` must be \"load\" or an HTTP-date",
        ),
        (BAD_PROXY_MODE, "unknown proxy mode `bogus`"),
    ];
    for (stub, refusal) in cases {
        let door = serde_json::from_str::<Stub>(stub)
            .expect_err("the validating decode refuses")
            .to_string();
        assert!(door.contains(refusal), "door: {door}");

        let stub_value: Stub = replayed(stub).expect("the replayed decode admits the bytes");
        let checked = admission_check_stub(&stub_value).expect_err("admission_check_stub refuses");
        assert!(
            checked.starts_with(refusal),
            "admission_check_stub: {checked}"
        );
        assert!(
            door.contains(&checked),
            "admission_check_stub must answer the door's message: {checked} vs {door}"
        );

        let config: ImposterConfig =
            replayed(&imposter_with(stub)).expect("the replayed decode admits the imposter");
        assert_eq!(admission_check(&config), Err(checked));
        assert!(serde_json::from_str::<ImposterConfig>(&imposter_with(stub)).is_err());
    }
}

#[test]
fn an_admitted_config_passes_admission_check() {
    let config: ImposterConfig = serde_json::from_str(&imposter_with(GOOD)).expect("admitted");
    assert_eq!(admission_check(&config), Ok(()));
}

/// The replay path normalises exactly as the door does: the same value for every config in the
/// conformance corpus, each of which also passes `admission_check`.
#[test]
fn the_corpus_decodes_identically_on_both_paths() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk-conformance/corpus/imposters");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("read the corpus") {
        let path = entry.expect("corpus entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read corpus file");
        let checked: ImposterConfig = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: validating decode: {e}", path.display()));
        let replay: ImposterConfig =
            replayed(&text).unwrap_or_else(|e| panic!("{}: replayed decode: {e}", path.display()));
        assert_eq!(
            serde_json::to_value(&replay).expect("serialize replayed"),
            serde_json::to_value(&checked).expect("serialize checked"),
            "{}",
            path.display()
        );
        assert_eq!(admission_check(&replay), Ok(()), "{}", path.display());
        seen += 1;
    }
    assert!(
        seen >= 10,
        "the corpus must not be silently empty: {seen} files"
    );
}

#[test]
fn a_validating_decode_after_a_replayed_one_still_refuses() {
    let _: Stub = replayed(BAD_JSONPATH).expect("replayed");
    assert!(serde_json::from_str::<Stub>(BAD_JSONPATH).is_err());
}

struct Boom;

impl<'de> Deserialize<'de> for Boom {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        panic!("decode panics mid-replay");
    }
}

#[test]
fn a_panic_inside_a_replayed_decode_does_not_leave_the_checks_off() {
    let outcome = std::panic::catch_unwind(|| replayed::<Boom>("null"));
    assert!(outcome.is_err(), "the decode panicked");
    assert!(serde_json::from_str::<Stub>(BAD_JSONPATH).is_err());
}

/// A replayed field inside a validating decode: the field skips the checks, its sibling does not.
#[derive(Deserialize)]
struct Envelope {
    #[serde(deserialize_with = "deserialize_replayed")]
    #[allow(dead_code)]
    stored: Stub,
    #[allow(dead_code)]
    submitted: Stub,
}

#[test]
fn a_replayed_field_does_not_switch_the_checks_off_for_its_siblings() {
    let admitted = format!(r#"{{ "stored": {BAD_JSONPATH}, "submitted": {GOOD} }}"#);
    assert!(serde_json::from_str::<Envelope>(&admitted).is_ok());
    let refused = format!(r#"{{ "stored": {GOOD}, "submitted": {BAD_COPY} }}"#);
    assert!(serde_json::from_str::<Envelope>(&refused).is_err());
}

/// A nested replayed decode restores the outer replay, not "checks on".
#[derive(Deserialize)]
struct Nested {
    #[serde(deserialize_with = "deserialize_replayed")]
    #[allow(dead_code)]
    inner: Stub,
    #[allow(dead_code)]
    after: Stub,
}

#[test]
fn a_nested_replayed_decode_keeps_the_outer_replay() {
    let json = format!(r#"{{ "inner": {GOOD}, "after": {BAD_MATCHES} }}"#);
    assert!(replayed::<Nested>(&json).is_ok());
}

/// `admission_check_stub` called from inside a replayed decode still checks.
struct Checked;

impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stub = Stub::deserialize(deserializer)?;
        admission_check_stub(&stub).map_err(serde::de::Error::custom)?;
        Ok(Checked)
    }
}

#[test]
fn admission_check_runs_even_inside_a_replayed_decode() {
    assert!(replayed::<Checked>(BAD_COPY).is_err());
    assert!(replayed::<Checked>(GOOD).is_ok());
}

/// The switch is per thread: a validating decode on another thread during a replay still checks.
struct OtherThreadRefuses(bool);

impl<'de> Deserialize<'de> for OtherThreadRefuses {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        let refused = std::thread::spawn(|| serde_json::from_str::<Stub>(BAD_JSONPATH).is_err())
            .join()
            .expect("thread joins");
        Ok(OtherThreadRefuses(refused))
    }
}

#[test]
fn the_switch_does_not_reach_other_threads() {
    let OtherThreadRefuses(refused) = replayed("null").expect("replayed");
    assert!(
        refused,
        "a validating decode on another thread must still refuse"
    );
}

#[test]
fn a_structural_refusal_holds_in_both_modes() {
    let stub = r#"{ "delayRange": [{ "min": 5, "max": 1 }],
                    "responses": [{ "is": { "statusCode": 200 } }] }"#;
    let message = "`delayRange` has min 5 greater than max 1";
    let checked = serde_json::from_str::<Stub>(stub)
        .expect_err("validating")
        .to_string();
    assert!(checked.contains(message), "{checked}");
    let replay = replayed::<Stub>(stub)
        .expect_err("replayed refuses too")
        .to_string();
    assert!(replay.contains(message), "{replay}");
}
