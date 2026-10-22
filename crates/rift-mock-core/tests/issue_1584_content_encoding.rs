//! Issue #1584: a request body sent with `Content-Encoding: gzip` or `br` is decoded before
//! matching and recording, as Mountebank's `httpRequest` does. A body that does not decode has no
//! body at all (Mountebank's `undefined`): predicates see it as missing, scripts see `undefined`,
//! and the journal records no `body` key. Any other coding value (`GZIP`, `gzip, br`, `deflate`)
//! is not decoded. The decoded size counts against the body limits (#1439), so a small
//! decompression bomb is a `413`, not an allocation.
//!
//! These drive the real serve loop (`ImposterManager` + reqwest), as `issue_636_binary_request_bodies.rs`
//! does, and pin every row of the triage's two probe tables against Mountebank 2.9.5's answers.

#![cfg(feature = "javascript")]

use rift_mock_core::imposter::body_limits::BodyLimits;
use rift_mock_core::imposter::{ImposterManager, ResponseMode};
use std::io::Write;
use std::time::Duration;

/// `brotliCompressSync("plain text")`, from Node 22's zlib.
const BR_PLAIN_TEXT: &[u8] = &[
    0x8b, 0x04, 0x80, 0x70, 0x6c, 0x61, 0x69, 0x6e, 0x20, 0x74, 0x65, 0x78, 0x74, 0x03,
];

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).expect("gzip into a Vec");
    encoder.finish().expect("gzip into a Vec")
}

async fn create(manager: &ImposterManager, cfg: serde_json::Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

async fn post(port: u16, path: &str, encoding: Option<&str>, body: Vec<u8>) -> (u16, String) {
    let mut request = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}{path}"))
        .body(body);
    if let Some(encoding) = encoding {
        request = request.header("Content-Encoding", encoding);
    }
    let response = request.send().await.expect("send");
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

/// The probe's imposter: first match wins, so the stub that answers says how the body was seen.
fn probe_imposter() -> serde_json::Value {
    serde_json::json!({
        "port": 0, "protocol": "http", "recordRequests": true,
        "stubs": [
            { "predicates": [{ "equals": { "body": "plain text" } }],
              "responses": [{ "is": { "body": "eq-plain" } }] },
            { "predicates": [{ "exists": { "body": false } }],
              "responses": [{ "is": { "body": "no-body" } }] },
            { "predicates": [{ "equals": { "body": "" } }],
              "responses": [{ "is": { "body": "empty-body" } }] },
            { "responses": [{ "is": { "body": "fallthrough" } }] }
        ]
    })
}

/// One probe row: the `Content-Encoding` sent, the body, the stub that answers, and the recorded
/// body (`None` is no `body` key).
type Row = (
    Option<&'static str>,
    Vec<u8>,
    &'static str,
    Option<&'static str>,
);

/// Every row of the triage's first table: which stub answers, and what is recorded.
#[tokio::test]
async fn content_encoded_bodies_match_and_record_as_mountebank_does() {
    let manager = ImposterManager::new();
    let port = create(&manager, probe_imposter()).await;
    let plain = b"plain text".to_vec();

    // (encoding, body, the stub that answers, the recorded body: `None` is no `body` key)
    let rows: Vec<Row> = vec![
        (Some("gzip"), plain.clone(), "no-body", None),
        (Some("br"), plain.clone(), "no-body", None),
        (
            Some("gzip"),
            gzip(b"plain text"),
            "eq-plain",
            Some("plain text"),
        ),
        (
            Some("br"),
            BR_PLAIN_TEXT.to_vec(),
            "eq-plain",
            Some("plain text"),
        ),
        (Some("GZIP"), plain.clone(), "eq-plain", Some("plain text")),
        (
            Some("gzip, br"),
            plain.clone(),
            "eq-plain",
            Some("plain text"),
        ),
        (
            Some("deflate"),
            plain.clone(),
            "eq-plain",
            Some("plain text"),
        ),
        (Some("gzip"), Vec::new(), "no-body", None),
        (None, plain.clone(), "eq-plain", Some("plain text")),
    ];

    for (i, (encoding, body, stub, _)) in rows.iter().enumerate() {
        let (status, answer) = post(port, &format!("/r{i}"), *encoding, body.clone()).await;
        assert_eq!(status, 200, "row {i} ({encoding:?})");
        assert_eq!(
            answer, *stub,
            "row {i} ({encoding:?}) answered by the wrong stub"
        );
    }

    let recorded = manager
        .get_imposter(port)
        .expect("imposter exists")
        .get_recorded_requests();
    assert_eq!(recorded.len(), rows.len());
    for (i, (encoding, _, _, expected)) in rows.iter().enumerate() {
        let request = recorded
            .iter()
            .find(|r| r.path == format!("/r{i}"))
            .unwrap_or_else(|| panic!("row {i} was recorded"));
        assert_eq!(
            request.body.as_deref(),
            *expected,
            "row {i} ({encoding:?}) recorded body"
        );
        // A decoded body that is UTF-8 is text, as Mountebank records it.
        assert_eq!(request.mode, ResponseMode::Text, "row {i}");
        let wire = serde_json::to_value(request).expect("serializes");
        assert_eq!(
            wire.get("body").is_some(),
            expected.is_some(),
            "row {i}: an undefined body has no `body` key on the wire: {wire}"
        );
        assert!(
            wire.get("_bodyOmitted").is_none(),
            "row {i}: an undefined body is not an omitted one: {wire}"
        );
        // Mountebank keeps the header the client sent.
        if let Some(encoding) = encoding {
            assert_eq!(
                request
                    .headers
                    .get("Content-Encoding")
                    .and_then(|v| v.first())
                    .map(String::as_str),
                Some(*encoding),
                "row {i}"
            );
        }
    }

    manager.delete_all().await;
}

/// The triage's second table: an undefined body as `equals {body: ""}`, an `inject` and a
/// `decorate` see it.
#[tokio::test]
async fn an_undefined_body_is_missing_to_predicates_and_scripts() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/e", "body": "" } }],
                  "responses": [{ "is": { "body": "matched-empty" } }] },
                { "predicates": [{ "equals": { "path": "/inject" } }],
                  "responses": [{ "inject":
                    "function (config) { return { body: typeof config.request.body + ':' + ('body' in config.request) }; }" }] },
                { "predicates": [{ "equals": { "path": "/decorate" } }],
                  "responses": [{ "is": { "body": "x" }, "behaviors": [{ "decorate":
                    "function (config) { config.response.body = typeof config.request.body; }" }] }] },
                { "predicates": [{ "equals": { "path": "/copy" } }],
                  "responses": [{ "is": { "body": "v=${v}" }, "behaviors": [{ "copy": {
                    "from": "body", "into": "${v}", "using": { "method": "regex", "selector": ".*" } } }] }] },
                { "responses": [{ "is": { "body": "fallthrough" } }] }
            ]
        }),
    )
    .await;
    let undecodable = || b"plain text".to_vec();

    let (_, answer) = post(port, "/e", Some("gzip"), undecodable()).await;
    assert_eq!(
        answer, "matched-empty",
        "equals {{body: \"\"}} matches an undefined body"
    );

    let (_, answer) = post(port, "/inject", Some("gzip"), undecodable()).await;
    assert_eq!(
        answer, "undefined:false",
        "inject sees no `body` on the request"
    );

    let (_, answer) = post(port, "/decorate", Some("gzip"), undecodable()).await;
    assert_eq!(
        answer, "undefined",
        "decorate sees no `body` on the request"
    );

    // Mountebank serves `v=undefined` here, through the missing-source rule #1583 adds; until
    // then the token is left. Either way the raw text the client sent is never what is copied.
    let (_, answer) = post(port, "/copy", Some("gzip"), undecodable()).await;
    assert_ne!(
        answer, "v=plain text",
        "copy must not see the undecoded text"
    );

    // A body that decodes is seen decoded by the same scripts.
    let (_, answer) = post(port, "/decorate", Some("gzip"), gzip(b"plain text")).await;
    assert_eq!(answer, "string");
    let (_, answer) = post(port, "/copy", Some("gzip"), gzip(b"plain text")).await;
    assert_eq!(answer, "v=plain text");

    manager.delete_all().await;
}

/// A decompression bomb: a 1 KiB gzip that inflates past the ceiling is a `413`, and the budget
/// the decoded bytes were charged to is whole again afterwards.
///
/// The budget is the ceiling plus a little: room for one decoded body at the ceiling beside the
/// small raw body it came from, and too little for a second one, so a leaked share would fail the
/// next request.
#[tokio::test]
async fn a_body_that_inflates_past_the_ceiling_is_refused_and_the_budget_returns() {
    const CEILING: usize = 64 * 1024;
    const BUDGET: usize = CEILING + 8 * 1024;
    let manager = ImposterManager::new();
    manager.set_body_limits(BodyLimits::new(CEILING, Some(BUDGET)).expect("valid limits"));
    let port = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "body": "ok" } }] }]
        }),
    )
    .await;

    let bomb = gzip(&vec![0u8; 1024 * 1024]);
    assert!(bomb.len() < 4 * 1024, "the bomb is small: {}", bomb.len());
    let (status, _) = post(port, "/", Some("gzip"), bomb).await;
    assert_eq!(status, 413, "the decoded size is what the ceiling limits");

    // Exactly at the ceiling still decodes and is served, which needs nearly the whole budget, so
    // neither the bomb's partial output nor this body's share may still be held after its answer.
    for attempt in 0..2 {
        let (status, answer) = post(port, "/", Some("gzip"), gzip(&vec![b'a'; CEILING])).await;
        assert_eq!((status, answer.as_str()), (200, "ok"), "attempt {attempt}");
    }

    manager.delete_all().await;
}

/// A proxy forwards the bytes the client sent, still encoded: the upstream decodes them itself.
#[tokio::test]
async fn a_proxy_forwards_the_encoded_bytes() {
    let manager = ImposterManager::new();
    let upstream = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [{ "responses": [{ "is": { "body": "upstream" } }] }]
        }),
    )
    .await;
    let front = create(
        &manager,
        serde_json::json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": {
                "to": format!("http://127.0.0.1:{upstream}"), "mode": "proxyTransparent" } }] }]
        }),
    )
    .await;

    let (status, answer) = post(front, "/ok", Some("gzip"), gzip(b"plain text")).await;
    assert_eq!((status, answer.as_str()), (200, "upstream"));
    let (status, answer) = post(front, "/bad", Some("gzip"), b"plain text".to_vec()).await;
    assert_eq!((status, answer.as_str()), (200, "upstream"));

    let recorded = manager
        .get_imposter(upstream)
        .expect("upstream exists")
        .get_recorded_requests();
    let ok = recorded.iter().find(|r| r.path == "/ok").expect("/ok");
    // Had the front forwarded the decoded text under the gzip header, this would not decode.
    assert_eq!(ok.body.as_deref(), Some("plain text"));
    let bad = recorded.iter().find(|r| r.path == "/bad").expect("/bad");
    assert_eq!(bad.body, None, "the upstream cannot decode it either");
    assert_eq!(
        bad.headers
            .get("Content-Length")
            .and_then(|v| v.first())
            .map(String::as_str),
        Some("10"),
        "the undecodable bytes were forwarded, not dropped"
    );

    manager.delete_all().await;
}
