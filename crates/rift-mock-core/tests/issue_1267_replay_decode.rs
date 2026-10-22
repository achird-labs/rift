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

/// An unknown generator `predicateOperator` (#1353): Mountebank and older engines admitted it, so it
/// is an admission check, never a decode refusal.
const BAD_GENERATOR_OPERATOR: &str = r#"{
    "responses": [{ "proxy": { "to": "http://localhost:1",
        "predicateGenerators": [{ "matches": { "path": true }, "predicateOperator": "bogus" }] } }]
}"#;

/// An unreadable generator entry (#1413): older engines skipped it, so it is an admission check.
const BAD_GENERATOR_SHAPE: &str = r#"{
    "responses": [{ "proxy": { "to": "http://localhost:1",
        "predicateGenerators": [{ "matches": { "path": true } }, { "caseSensitive": true }] } }]
}"#;

const BAD_GENERATOR_MATCH_FIELD: &str = r#"{
    "responses": [{ "proxy": { "to": "http://localhost:1",
        "predicateGenerators": [{ "matches": { "pathh": true } }] } }]
}"#;

/// A `proxy.to` with a path (#1550): every engine before it admitted and stored one, so it is an
/// admission check, never a decode refusal.
const BAD_PROXY_TO_PATH: &str = r#"{ "responses": [{ "proxy": { "to": "http://h/x" } }] }"#;

/// A `proxy.to` that is not a URL (#1550), likewise admitted and stored before.
const BAD_PROXY_TO_URL: &str = r#"{ "responses": [{ "proxy": { "to": "not a url" } }] }"#;

/// A stub with no responses (issue #1372): engines up to v0.21.0 admitted it on every door and
/// served the imposter's `defaultResponse`, so it is an admission check, never a decode refusal.
const NO_RESPONSES: &str = r#"{ "predicates": [] }"#;

const EMPTY_RESPONSES: &str =
    r#"{ "predicates": [{ "equals": { "path": "/a" } }], "responses": [] }"#;

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

/// An XPath `copy` whose `ns` is not a prefix→URI object (issue #1326). The engine ignored `ns`
/// before #1326, so such bytes may be stored and must still decode on replay.
const BAD_COPY_NS: &str = r#"{
    "responses": [{
        "is": { "statusCode": 200, "body": "${T}" },
        "_behaviors": { "copy": { "from": "body", "into": "${T}",
                                   "using": { "method": "xpath", "selector": "//a:b", "ns": "urn:a" } } }
    }]
}"#;

/// Issue #1582: a `repeat` of `0`, which Mountebank refuses and engines up to v0.23.0 admitted and
/// served as `1`. Door-only: an older engine stored it, so a store must replay it.
const ZERO_REPEAT: &str = r#"{ "responses": [{ "is": { "statusCode": 200 }, "repeat": 0 }] }"#;

/// Issue #1582: a `copy.from` with two keys (Rift read `query` and ignored the rest), and a lookup
/// `key.from` with none. Door-only: stored as written by older engines, so a store must replay them.
const TWO_KEY_COPY_FROM: &str = r#"{
    "responses": [{
        "is": { "statusCode": 200, "body": "${T}" },
        "_behaviors": { "copy": { "from": { "query": "q", "headers": "x" }, "into": "${T}",
                                   "using": { "method": "regex", "selector": ".+" } } }
    }]
}"#;

const EMPTY_LOOKUP_FROM: &str = r#"{
    "responses": [{
        "is": { "statusCode": 200, "body": "${R}" },
        "behaviors": [{ "lookup": { "key": { "from": {}, "using": { "method": "regex", "selector": ".+" } },
                                    "fromDataSource": { "csv": { "path": "x.csv", "keyColumn": "id" } },
                                    "into": "${R}" } }]
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
        (
            BAD_GENERATOR_OPERATOR,
            "predicateGenerators[0].predicateOperator `bogus` is not a predicate operator",
        ),
        (
            BAD_GENERATOR_SHAPE,
            "predicateGenerators[1] needs `matches` or `inject`",
        ),
        (
            BAD_GENERATOR_MATCH_FIELD,
            "predicateGenerators[0].matches field `pathh` is not a request field",
        ),
        (
            BAD_COPY_NS,
            "`copy` behavior `xpath` `using.ns` must be an object mapping each prefix to a namespace URI string",
        ),
        (BAD_PROXY_TO_PATH, "proxy.to must not contain a path '/x'"),
        (BAD_PROXY_TO_URL, "proxy.to is not a valid URL: not a url"),
        (NO_RESPONSES, "'responses' must be a non-empty array"),
        (EMPTY_RESPONSES, "'responses' must be a non-empty array"),
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

/// Issue #1372: an absent or empty `responses` is the new admission check; an explicit
/// `"responses": null` was always a structural refusal and stays one under both modes.
#[test]
fn null_responses_stays_a_structural_refusal_on_replay() {
    let stub = r#"{ "predicates": [], "responses": null }"#;
    assert!(serde_json::from_str::<Stub>(stub).is_err(), "validating");
    assert!(replayed::<Stub>(stub).is_err(), "replayed refuses too");
}

/// Issue #1372: a stub an older engine admitted without responses still decodes on replay, and
/// `admission_check` names the rule.
#[test]
fn a_replayed_stub_without_responses_still_decodes() {
    for stub in [NO_RESPONSES, EMPTY_RESPONSES] {
        let decoded: Stub = replayed(stub).expect("replay admits");
        assert!(decoded.responses.is_empty());
        let config: ImposterConfig = replayed(&imposter_with(stub)).expect("replay admits");
        assert_eq!(
            admission_check(&config),
            Err("'responses' must be a non-empty array".to_string())
        );
    }
}

/// Issue #1551: a response with two types, or none, is a door-only refusal. Engines up to v0.23.0
/// admitted both and kept the first type by precedence, or served `{}` as an empty `is`; an embedder
/// that stored such bytes still decodes them to that value, and `admission_check` passes on it,
/// because the decoded response holds exactly one type.
#[test]
fn a_response_with_two_types_or_none_replays_to_its_pre_1551_value() {
    let cases = [
        (
            r#"{ "responses": [{ "is": { "statusCode": 201 }, "proxy": { "to": "http://127.0.0.1:9" } }] }"#,
            "each response object must have only one response type",
            serde_json::json!({ "is": { "statusCode": 201 } }),
        ),
        (
            r#"{ "responses": [{}] }"#,
            "unrecognized response type",
            serde_json::json!({ "is": {} }),
        ),
        (
            r#"{ "responses": [{ "behaviors": [{ "wait": 10 }] }] }"#,
            "unrecognized response type",
            serde_json::json!({ "is": {}, "behaviors": [{ "wait": 10 }] }),
        ),
    ];
    for (stub, refusal, replays_to) in cases {
        let door = serde_json::from_str::<Stub>(stub)
            .expect_err("the validating decode refuses")
            .to_string();
        assert!(door.starts_with(refusal), "door: {door}");

        let decoded: Stub = replayed(stub).expect("the replayed decode admits the bytes");
        assert_eq!(
            serde_json::to_value(&decoded.responses[0]).expect("print"),
            replays_to,
            "{stub}"
        );
        assert_eq!(admission_check_stub(&decoded), Ok(()), "door-only: {stub}");
        let config: ImposterConfig = replayed(&imposter_with(stub)).expect("replay admits");
        assert_eq!(admission_check(&config), Ok(()), "{stub}");
    }
}

/// Issue #1578: `"mode": "binary"` refuses `matches` and `jsonpath`/`xpath` selectors. The refusal is
/// an imposter-level admission check, never a decode refusal: both decodes admit the bytes, and
/// `admission_check` reports it.
const BINARY_MATCHES: &str = r#"{
    "port": 4700, "protocol": "http", "mode": "binary",
    "stubs": [{ "predicates": [{ "matches": { "body": "^AQ" } }],
                "responses": [{ "is": { "statusCode": 200 } }] }]
}"#;

#[test]
fn a_binary_mode_imposter_is_refused_by_admission_check_not_by_the_decode() {
    let message = "the matches predicate is not allowed in binary mode";
    let door: ImposterConfig =
        serde_json::from_str(BINARY_MATCHES).expect("the validating decode admits the bytes");
    assert_eq!(admission_check(&door), Err(message.to_string()));
    let replay: ImposterConfig =
        replayed(BINARY_MATCHES).expect("the replayed decode admits the bytes");
    assert_eq!(admission_check(&replay), Err(message.to_string()));
}

/// `mode` is never serialized, so what an engine stores for a binary imposter carries no `mode` and
/// can never newly fail `admission_check` on replay: the refusal is unreachable there by construction.
#[test]
fn a_stored_binary_mode_imposter_replays_without_mode_and_passes_admission_check() {
    let door: ImposterConfig = serde_json::from_str(BINARY_MATCHES).expect("decode");
    let stored = serde_json::to_string(&door).expect("serialize");
    assert!(!stored.contains("\"mode\""), "stored: {stored}");
    let replay: ImposterConfig = replayed(&stored).expect("the stored bytes replay");
    assert_eq!(admission_check(&replay), Ok(()));
}

/// The door refuses a stub-level failure while decoding, before the imposter-level binary check
/// runs, so `admission_check` names it first even when it sits in a later stub.
#[test]
fn a_stub_level_refusal_is_reported_before_the_binary_mode_one() {
    let config = format!(
        r#"{{ "port": 4700, "protocol": "http", "mode": "binary", "stubs": [
            {{ "predicates": [{{ "matches": {{ "body": "x" }} }}],
               "responses": [{{ "is": {{ "statusCode": 200 }} }}] }},
            {BAD_MATCHES}
        ] }}"#
    );
    let replay: ImposterConfig = replayed(&config).expect("the replayed decode admits the bytes");
    let checked = admission_check(&replay).expect_err("admission_check refuses");
    assert!(
        checked.starts_with("predicate `matches` field `path` has an invalid regex"),
        "admission_check: {checked}"
    );
}

/// Issue #1582: every behaviors refusal it added is door-only. `compile_behaviors` splits a
/// multi-key element into one step per key and drops a `null` `repeat`, and engines up to v0.23.0
/// stored a `0` `repeat` and a `from` of any size, so these replay to the value those engines stored
/// and `admission_check` passes: a store (rift-cluster's log, `--datadir`) never drops them.
#[test]
fn the_door_only_behavior_shapes_replay_to_their_pre_1582_value() {
    let cases = [
        (
            ZERO_REPEAT,
            "\"repeat\" field must be an integer greater than 0",
            serde_json::json!({ "is": { "statusCode": 200 } }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "_behaviors": { "wait": 1, "repeat": 0 } }] }"#,
            "\"repeat\" field must be an integer greater than 0",
            serde_json::json!({ "is": {}, "behaviors": [{ "wait": 1 }] }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "behaviors": [{ "repeat": 0 }] }] }"#,
            "Unrecognized behavior: \"repeat\"",
            serde_json::json!({ "is": {} }),
        ),
        (
            TWO_KEY_COPY_FROM,
            "copy behavior \"from\" field must have exactly one key",
            serde_json::json!({
                "is": { "statusCode": 200, "body": "${T}" },
                "behaviors": [{ "copy": { "from": { "query": "q", "headers": "x" }, "into": "${T}",
                                          "using": { "method": "regex", "selector": ".+" } } }]
            }),
        ),
        (
            EMPTY_LOOKUP_FROM,
            "lookup behavior \"key.from\" field must have exactly one key",
            serde_json::json!({
                "is": { "statusCode": 200, "body": "${R}" },
                "behaviors": [{ "lookup": {
                    "key": { "from": {}, "using": { "method": "regex", "selector": ".+" } },
                    "fromDataSource": { "csv": { "path": "x.csv", "keyColumn": "id" } },
                    "into": "${R}" } }]
            }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "repeat": null }] }"#,
            "\"repeat\" field must be an integer greater than 0",
            serde_json::json!({ "is": {} }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "_behaviors": { "wait": 1, "repeat": null } }] }"#,
            "\"repeat\" field must be an integer greater than 0",
            serde_json::json!({ "is": {}, "behaviors": [{ "wait": 1 }] }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "behaviors": [{ "wait": 1 }, { "repeat": 2 }] }] }"#,
            "Unrecognized behavior: \"repeat\"",
            serde_json::json!({ "is": {}, "repeat": 2, "behaviors": [{ "wait": 1 }] }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "behaviors": [{ "wait": 1 }, { "nosuch": 1 }] }] }"#,
            "Unrecognized behavior: \"nosuch\"",
            serde_json::json!({ "is": {}, "behaviors": [{ "wait": 1, "nosuch": 1 }] }),
        ),
        (
            r#"{ "responses": [{ "is": {}, "behaviors": [{ "wait": 1, "decorate": "function () {}" }] }] }"#,
            "Each behavior object must have only one behavior type",
            serde_json::json!({ "is": {}, "behaviors": [{ "wait": 1 }, { "decorate": "function () {}" }] }),
        ),
    ];
    for (stub, refusal, replays_to) in cases {
        let door = serde_json::from_str::<Stub>(stub)
            .expect_err("the validating decode refuses")
            .to_string();
        assert!(door.starts_with(refusal), "door: {door}");

        let decoded: Stub = replayed(stub).expect("the replayed decode admits the bytes");
        assert_eq!(
            serde_json::to_value(&decoded.responses[0]).expect("print"),
            replays_to,
            "{stub}"
        );
        assert_eq!(admission_check_stub(&decoded), Ok(()), "door-only: {stub}");
        let config: ImposterConfig = replayed(&imposter_with(stub)).expect("replay admits");
        assert_eq!(admission_check(&config), Ok(()), "{stub}");
    }
}

/// Issue #1582: a lookup `key.index` that is not a match position (Mountebank reads it as a
/// property name and finds no row) is admitted on both paths, and prints back as written.
#[test]
fn any_lookup_key_index_decodes_on_both_paths() {
    for index in ["-1", "\"1\"", "1.5", "{}"] {
        let stub = format!(
            r#"{{ "responses": [{{ "is": {{ "body": "${{R}}" }},
                 "_behaviors": {{ "lookup": {{
                   "key": {{ "from": "path", "using": {{ "method": "regex", "selector": ".+" }},
                            "index": {index} }},
                   "fromDataSource": {{ "csv": {{ "path": "x.csv", "keyColumn": "id" }} }},
                   "into": "${{R}}" }} }} }}] }}"#
        );
        let checked: Stub = serde_json::from_str(&stub).expect("the door admits it");
        let replay: Stub = replayed(&stub).expect("the replayed decode admits it");
        let printed = serde_json::to_value(&checked).expect("print");
        assert_eq!(printed, serde_json::to_value(&replay).expect("print"));
        assert_eq!(
            printed["responses"][0]["behaviors"][0]["lookup"]["key"]["index"],
            serde_json::from_str::<serde_json::Value>(index).expect("json"),
        );
        assert_eq!(admission_check_stub(&checked), Ok(()));
    }
}
