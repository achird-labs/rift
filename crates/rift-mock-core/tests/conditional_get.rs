//! Issue #1280: `_rift.conditional` on an `is` response adds a strong `ETag` (FNV-1a 64 over the
//! served bytes) and a `Last-Modified`, and answers `304 Not Modified` to a matching
//! `If-None-Match` / `If-Modified-Since` (RFC 7232). These tests drive the real serve loop
//! (`ImposterManager` + reqwest) on both the prepared fast path (a static body) and the slow path
//! (templated bodies, behaviors).
//!
//! ETags asserted literally are FNV-1a 64 of the body bytes, computed outside this code.

use rift_mock_core::imposter::{ImposterManager, Stub, admission_check_stub, deserialize_replayed};
use serde_json::{Value, json};
use std::time::Duration;

const HELLO_ETAG: &str = "\"fnv1a64-a430d84680aabd0b\"";
const FIXED_DATE: &str = "Sat, 03 Oct 2026 12:00:00 GMT";

async fn create(manager: &ImposterManager, cfg: Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// One imposter with a single stub answering every request with `response`.
async fn serving(manager: &ImposterManager, response: Value) -> u16 {
    create(
        manager,
        json!({ "port": 0, "protocol": "http", "stubs": [{ "responses": [response] }] }),
    )
    .await
}

fn hello(conditional: Value) -> Value {
    json!({
        "is": {
            "statusCode": 200,
            "headers": {
                "Content-Type": "text/plain",
                "Cache-Control": "max-age=60",
                "Vary": "Accept-Encoding",
                "X-Other": "kept-off-304"
            },
            "body": "hello"
        },
        "_rift": { "conditional": conditional }
    })
}

async fn send(
    method: reqwest::Method,
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request =
        reqwest::Client::new().request(method, format!("http://127.0.0.1:{port}{path}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.expect("send")
}

async fn get(port: u16, headers: &[(&str, &str)]) -> reqwest::Response {
    send(reqwest::Method::GET, port, "/", headers).await
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii header").to_owned())
}

fn assert_etag_format(etag: &str) {
    let re = regex::Regex::new(r#"^"fnv1a64-[0-9a-f]{16}"$"#).expect("valid regex");
    assert!(re.is_match(etag), "not a rift ETag: {etag}");
}

fn assert_http_date(value: &str) {
    let re = regex::Regex::new(r"^[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$")
        .expect("valid regex");
    assert!(re.is_match(value), "not an IMF-fixdate: {value}");
}

#[tokio::test]
async fn conditional_static_body_200_with_etag_on_first_get() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let response = get(port, &[]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "etag").as_deref(), Some(HELLO_ETAG));
    let last_modified = header(&response, "last-modified").expect("Last-Modified on a 200");
    assert_http_date(&last_modified);
    assert_eq!(response.text().await.expect("body"), "hello");
    manager.delete_all().await;
}

#[tokio::test]
async fn conditional_static_body_304_on_matching_if_none_match() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let response = get(port, &[("If-None-Match", HELLO_ETAG)]).await;
    assert_eq!(response.status(), 304);
    assert_eq!(header(&response, "etag").as_deref(), Some(HELLO_ETAG));
    assert!(header(&response, "last-modified").is_some());
    assert_eq!(
        header(&response, "cache-control").as_deref(),
        Some("max-age=60")
    );
    assert_eq!(
        header(&response, "vary").as_deref(),
        Some("Accept-Encoding")
    );
    assert_eq!(
        header(&response, "x-rift-imposter").as_deref(),
        Some("true")
    );
    assert_eq!(header(&response, "content-type"), None);
    assert_eq!(header(&response, "content-length"), None);
    assert_eq!(header(&response, "x-other"), None);
    assert_eq!(response.text().await.expect("body"), "");

    let head = send(
        reqwest::Method::HEAD,
        port,
        "/",
        &[("If-None-Match", HELLO_ETAG)],
    )
    .await;
    assert_eq!(head.status(), 304, "HEAD is conditional too");

    let other = get(port, &[("If-None-Match", "\"fnv1a64-0000000000000000\"")]).await;
    assert_eq!(other.status(), 200);
    assert_eq!(other.text().await.expect("body"), "hello");
    manager.delete_all().await;
}

#[tokio::test]
async fn conditional_static_body_304_on_if_modified_since_at_or_after_load() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let first = get(port, &[]).await;
    let last_modified = header(&first, "last-modified").expect("Last-Modified");

    let same = get(port, &[("If-Modified-Since", last_modified.as_str())]).await;
    assert_eq!(same.status(), 304);
    assert_eq!(
        header(&same, "last-modified").as_deref(),
        Some(last_modified.as_str())
    );

    let later = get(
        port,
        &[("If-Modified-Since", "Sat, 01 Jan 2050 00:00:00 GMT")],
    )
    .await;
    assert_eq!(later.status(), 304);
    manager.delete_all().await;
}

#[tokio::test]
async fn conditional_static_body_200_on_stale_if_modified_since() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let stale = get(
        port,
        &[("If-Modified-Since", "Sat, 01 Jan 2000 00:00:00 GMT")],
    )
    .await;
    assert_eq!(stale.status(), 200);
    assert_eq!(stale.text().await.expect("body"), "hello");

    let garbage = get(port, &[("If-Modified-Since", "not a date")]).await;
    assert_eq!(garbage.status(), 200, "an unparseable date is ignored");
    manager.delete_all().await;
}

#[tokio::test]
async fn if_none_match_takes_precedence_over_if_modified_since() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let mismatch = get(
        port,
        &[
            ("If-None-Match", "\"something-else\""),
            ("If-Modified-Since", "Sat, 01 Jan 2050 00:00:00 GMT"),
        ],
    )
    .await;
    assert_eq!(
        mismatch.status(),
        200,
        "If-Modified-Since is ignored when If-None-Match is present"
    );

    let matching = get(
        port,
        &[
            ("If-None-Match", HELLO_ETAG),
            ("If-Modified-Since", "Sat, 01 Jan 2000 00:00:00 GMT"),
        ],
    )
    .await;
    assert_eq!(matching.status(), 304);
    manager.delete_all().await;
}

#[tokio::test]
async fn weak_and_list_etags_match() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    for value in [
        "W/\"fnv1a64-a430d84680aabd0b\"",
        "\"nope\", \"fnv1a64-a430d84680aabd0b\"",
        "\"a,b\",W/\"fnv1a64-a430d84680aabd0b\"",
        "*",
    ] {
        let response = get(port, &[("If-None-Match", value)]).await;
        assert_eq!(response.status(), 304, "If-None-Match: {value}");
    }
    let partial = get(port, &[("If-None-Match", "\"fnv1a64-a430d84680aabd0\"")]).await;
    assert_eq!(partial.status(), 200);
    manager.delete_all().await;
}

#[tokio::test]
async fn post_is_never_304() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;

    let response = send(
        reqwest::Method::POST,
        port,
        "/",
        &[("If-None-Match", HELLO_ETAG)],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "etag"), None, "served unchanged");
    assert_eq!(header(&response, "last-modified"), None, "served unchanged");
    assert_eq!(response.text().await.expect("body"), "hello");
    manager.delete_all().await;
}

#[tokio::test]
async fn non_2xx_is_never_304() {
    let manager = ImposterManager::new();
    let port = serving(
        &manager,
        json!({ "is": { "statusCode": 404, "body": "gone" }, "_rift": { "conditional": true } }),
    )
    .await;

    let response = get(port, &[("If-None-Match", "*")]).await;
    assert_eq!(response.status(), 404);
    assert_eq!(header(&response, "etag"), None);
    assert_eq!(response.text().await.expect("body"), "gone");
    manager.delete_all().await;
}

#[tokio::test]
async fn templated_body_gets_per_request_etag() {
    let manager = ImposterManager::new();
    let port = serving(
        &manager,
        json!({
            "is": { "statusCode": 200, "headers": { "Cache-Control": "no-cache" },
                    "body": "path=${request.path}" },
            "_rift": { "conditional": { "etag": true, "lastModified": "load" } }
        }),
    )
    .await;

    let a = send(reqwest::Method::GET, port, "/a", &[]).await;
    let a_etag = header(&a, "etag").expect("ETag");
    let b = send(reqwest::Method::GET, port, "/b", &[]).await;
    let b_etag = header(&b, "etag").expect("ETag");
    assert_etag_format(&a_etag);
    assert_etag_format(&b_etag);
    assert_ne!(a_etag, b_etag);
    assert_eq!(a_etag, "\"fnv1a64-d5f0ab6bba55fa21\"");
    assert_eq!(b_etag, "\"fnv1a64-d5f0a86bba55f508\"");

    let again = send(
        reqwest::Method::GET,
        port,
        "/a",
        &[("If-None-Match", a_etag.as_str())],
    )
    .await;
    assert_eq!(again.status(), 304);
    assert_eq!(header(&again, "etag"), Some(a_etag.clone()));
    assert_eq!(header(&again, "cache-control").as_deref(), Some("no-cache"));
    assert_eq!(header(&again, "content-length"), None);

    let other = send(
        reqwest::Method::GET,
        port,
        "/b",
        &[("If-None-Match", a_etag.as_str())],
    )
    .await;
    assert_eq!(other.status(), 200);
    assert_eq!(other.text().await.expect("body"), "path=/b");
    manager.delete_all().await;
}

#[tokio::test]
async fn behavior_altered_body_hashes_the_final_bytes() {
    let manager = ImposterManager::new();
    let port = serving(
        &manager,
        json!({
            "is": { "statusCode": 200, "body": "value=${V}" },
            "_behaviors": { "copy": { "from": { "query": "v" }, "into": "${V}",
                                      "using": { "method": "regex", "selector": ".+" } } },
            "_rift": { "conditional": true }
        }),
    )
    .await;

    let a = send(reqwest::Method::GET, port, "/?v=a", &[]).await;
    assert_eq!(
        header(&a, "etag").as_deref(),
        Some("\"fnv1a64-a451eee9b23a7b5c\"")
    );
    assert_eq!(a.text().await.expect("body"), "value=a");

    let b = send(
        reqwest::Method::GET,
        port,
        "/?v=b",
        &[("If-None-Match", "\"fnv1a64-a451eee9b23a7b5c\"")],
    )
    .await;
    assert_eq!(b.status(), 200);
    assert_eq!(
        header(&b, "etag").as_deref(),
        Some("\"fnv1a64-a451f1e9b23a8075\"")
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn fixed_last_modified_literal_is_served_verbatim() {
    let manager = ImposterManager::new();
    let port = serving(
        &manager,
        hello(json!({ "etag": false, "lastModified": FIXED_DATE })),
    )
    .await;

    let first = get(port, &[]).await;
    assert_eq!(first.status(), 200);
    assert_eq!(header(&first, "last-modified").as_deref(), Some(FIXED_DATE));
    assert_eq!(header(&first, "etag"), None, "etag: false");

    let same = get(port, &[("If-Modified-Since", FIXED_DATE)]).await;
    assert_eq!(same.status(), 304);
    assert_eq!(header(&same, "last-modified").as_deref(), Some(FIXED_DATE));
    assert_eq!(header(&same, "etag"), None);

    let before = get(
        port,
        &[("If-Modified-Since", "Sat, 03 Oct 2026 11:59:59 GMT")],
    )
    .await;
    assert_eq!(before.status(), 200);
    manager.delete_all().await;
}

#[tokio::test]
async fn a_304_consumes_a_cycle_position() {
    let manager = ImposterManager::new();
    let port = serving_two(&manager).await;

    let first = get(port, &[]).await;
    assert_eq!(first.text().await.expect("body"), "v1");
    let second = get(port, &[]).await;
    assert_eq!(second.text().await.expect("body"), "v2");
    let third = get(port, &[("If-None-Match", "\"fnv1a64-08cf0b07b5709128\"")]).await;
    assert_eq!(third.status(), 304, "v1's turn, and v1's ETag");
    let fourth = get(port, &[]).await;
    assert_eq!(
        fourth.text().await.expect("body"),
        "v2",
        "the 304 used v1's position"
    );
    manager.delete_all().await;
}

async fn serving_two(manager: &ImposterManager) -> u16 {
    create(
        manager,
        json!({ "port": 0, "protocol": "http", "stubs": [{ "responses": [
            { "is": { "body": "v1" }, "_rift": { "conditional": true } },
            { "is": { "body": "v2" }, "_rift": { "conditional": true } }
        ] }] }),
    )
    .await
}

#[tokio::test]
async fn journal_records_304_as_served_status() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "recordRequests": true,
                "stubs": [{ "responses": [hello(json!(true))] }] }),
    )
    .await;

    let response = get(port, &[("If-None-Match", HELLO_ETAG)]).await;
    assert_eq!(response.status(), 304);

    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1);
    let entry = serde_json::to_value(&recorded[0]).expect("serializes");
    assert_eq!(entry["status"], 304, "{entry}");
    manager.delete_all().await;
}

#[tokio::test]
async fn cors_headers_present_on_304() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "allowCORS": true,
                "stubs": [{ "responses": [hello(json!(true))] }] }),
    )
    .await;

    let response = get(port, &[("If-None-Match", HELLO_ETAG)]).await;
    assert_eq!(response.status(), 304);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("*")
    );
    manager.delete_all().await;
}

#[tokio::test]
async fn datadir_persists_conditional_exactly_as_declared() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
    let shapes = [
        json!(true),
        json!({ "etag": true, "lastModified": "load" }),
        json!({ "etag": false, "lastModified": FIXED_DATE }),
        json!({ "etag": false }),
    ];
    let stubs: Vec<Value> = shapes
        .iter()
        .map(|shape| json!({ "responses": [hello(shape.clone())] }))
        .collect();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "stubs": stubs }),
    )
    .await;

    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(format!("{port}.json"))).expect("datadir file"),
    )
    .expect("json");
    for (i, shape) in shapes.iter().enumerate() {
        assert_eq!(
            saved["stubs"][i]["responses"][0]["_rift"]["conditional"], *shape,
            "{saved}"
        );
    }
    manager.delete_all().await;
}

#[test]
fn an_absent_conditional_is_not_echoed() {
    let stub: Stub = serde_json::from_value(json!({
        "responses": [{ "is": { "body": "x" }, "_rift": { "templated": true } }]
    }))
    .expect("decodes");
    let echoed = serde_json::to_value(&stub).expect("serializes");
    assert!(
        echoed["responses"][0]["_rift"].get("conditional").is_none(),
        "{echoed}"
    );
}

#[test]
fn bad_fixed_date_is_refused_at_admission_not_decode() {
    let stub = r#"{"responses":[{"is":{"body":"x"},"_rift":{"conditional":{"lastModified":"yesterday"}}}]}"#;
    let refusal = "`_rift.conditional.lastModified` must be \"load\" or an HTTP-date such as \
                   \"Sat, 03 Oct 2026 12:00:00 GMT\"; got \"yesterday\"";

    let door = serde_json::from_str::<Stub>(stub)
        .expect_err("a config door refuses")
        .to_string();
    assert!(door.contains(refusal), "door: {door}");

    let mut deserializer = serde_json::Deserializer::from_str(stub);
    let replayed: Stub =
        deserialize_replayed(&mut deserializer).expect("a replayed decode still decodes it");
    assert_eq!(admission_check_stub(&replayed), Err(refusal.to_owned()));
}

// ===== Review follow-ups: ordering against fault and stateOps, validator replacement, parity =====

/// A fault decides the response before conditional does: a request that would match is still
/// answered with the injected error.
#[tokio::test]
async fn a_fault_wins_over_conditional() {
    let manager = ImposterManager::new();
    let mut response = hello(json!(true));
    response["_rift"]["fault"] = json!({ "error": { "probability": 1.0, "status": 503 } });
    let port = serving(&manager, response).await;
    let response = get(port, &[("If-None-Match", "*")]).await;
    assert_eq!(response.status(), 503, "the fault, not a 304");
    assert_eq!(header(&response, "etag"), None);
    manager.delete_all().await;
}

/// `stateOps` still run when the answer is a 304: polling advances state like any other request.
#[tokio::test]
async fn state_ops_run_on_a_304() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http",
            "_rift": { "flowState": { "flowIdSource": "header:X-Session" } },
            "stubs": [{ "responses": [{
                "is": { "statusCode": 200, "body": "hello" },
                "_rift": { "conditional": true,
                           "stateOps": [{ "op": "increment", "key": "polls" }] }
            }] }]
        }),
    )
    .await;
    for _ in 0..2 {
        let response = get(port, &[("If-None-Match", "*"), ("X-Session", "s-1280")]).await;
        assert_eq!(response.status(), 304);
    }
    let polls = manager
        .get_imposter(port)
        .expect("imposter")
        .flow_store
        .get("s-1280", "polls")
        .expect("store read");
    assert_eq!(polls, Some(json!(2)));
    manager.delete_all().await;
}

/// A configured `ETag` / `Last-Modified` is replaced by the generated one, never sent alongside
/// it, on both the prepared path (static) and the slow path (templated) — which also agree on the
/// ETag of the same bytes.
#[tokio::test]
async fn configured_validators_are_replaced_on_both_paths() {
    const STALE_DATE: &str = "Mon, 01 Jan 2001 00:00:00 GMT";
    for templated in [false, true] {
        let manager = ImposterManager::new();
        let port = serving(
            &manager,
            json!({
                "is": { "statusCode": 200,
                        "headers": { "ETag": "\"stale\"", "Last-Modified": STALE_DATE,
                                     "Content-Type": "text/plain" },
                        "body": "hello" },
                "_rift": { "conditional": true, "templated": templated }
            }),
        )
        .await;
        let response = get(port, &[]).await;
        assert_eq!(response.status(), 200, "templated={templated}");
        let etags: Vec<_> = response.headers().get_all("etag").iter().collect();
        assert_eq!(etags.len(), 1, "templated={templated}: {etags:?}");
        assert_eq!(etags[0], HELLO_ETAG, "templated={templated}");
        let dates: Vec<_> = response.headers().get_all("last-modified").iter().collect();
        assert_eq!(dates.len(), 1, "templated={templated}: {dates:?}");
        assert_ne!(dates[0], STALE_DATE, "templated={templated}");

        let not_modified = get(port, &[("If-None-Match", HELLO_ETAG)]).await;
        assert_eq!(not_modified.status(), 304, "templated={templated}");
        assert_eq!(
            header(&not_modified, "content-type"),
            None,
            "templated={templated}"
        );
        assert_eq!(
            header(&not_modified, "content-length"),
            None,
            "templated={templated}"
        );
        assert_eq!(
            not_modified.text().await.expect("body"),
            "",
            "templated={templated}"
        );
        manager.delete_all().await;
    }
}

/// HEAD's 200 carries the same validators as GET's, with no body.
#[tokio::test]
async fn head_200_carries_the_get_validators() {
    let manager = ImposterManager::new();
    let port = serving(&manager, hello(json!(true))).await;
    let head = send(reqwest::Method::HEAD, port, "/", &[]).await;
    assert_eq!(head.status(), 200);
    assert_eq!(header(&head, "etag").as_deref(), Some(HELLO_ETAG));
    manager.delete_all().await;
}

// ===== Issue #1294: an identical replace is not a change =====

/// A stub that answers "a" then "b" (so the cycle position is observable) with `conditional`.
fn cycling_stub(id: &str, first: &str) -> Value {
    json!({
        "id": id,
        "responses": [
            { "is": { "statusCode": 200, "body": first }, "_rift": { "conditional": true } },
            { "is": { "statusCode": 200, "body": "b" }, "_rift": { "conditional": true } }
        ]
    })
}

fn stub(v: Value) -> Stub {
    serde_json::from_value(v).expect("valid stub")
}

async fn last_modified_and_body(port: u16) -> (String, String) {
    let response = get(port, &[]).await;
    let lm = header(&response, "last-modified").expect("Last-Modified");
    (lm, response.text().await.expect("body"))
}

#[tokio::test]
async fn replace_stub_with_identical_content_keeps_last_modified_and_cursor() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "stubs": [cycling_stub("s", "a")] }),
    )
    .await;
    let (stamp, body) = last_modified_and_body(port).await;
    assert_eq!(body, "a");
    tokio::time::sleep(Duration::from_millis(1100)).await;

    manager
        .replace_stub(port, 0, stub(cycling_stub("s", "a")))
        .await
        .expect("replace");
    let (after, body) = last_modified_and_body(port).await;
    assert_eq!(after, stamp, "an identical replace is not a change");
    assert_eq!(body, "b", "the cycle position is kept, as it always was");
    manager.delete_all().await;
}

#[tokio::test]
async fn replace_stub_by_id_with_identical_content_keeps_last_modified() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "stubs": [cycling_stub("s", "a")] }),
    )
    .await;
    let (stamp, _) = last_modified_and_body(port).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    manager
        .replace_stub_by_id(port, "s", stub(cycling_stub("s", "a")))
        .await
        .expect("replace");
    let (after, _) = last_modified_and_body(port).await;
    assert_eq!(after, stamp);
    manager.delete_all().await;
}

/// The bulk replace keeps its pinned contract — every cycle restarts — while an unchanged stub
/// keeps its `Last-Modified`.
#[tokio::test]
async fn replace_stubs_with_an_identical_set_keeps_last_modified_but_resets_the_cycle() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "stubs": [cycling_stub("s", "a")] }),
    )
    .await;
    let (stamp, body) = last_modified_and_body(port).await;
    assert_eq!(body, "a");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    manager
        .replace_stubs(port, vec![stub(cycling_stub("s", "a"))])
        .await
        .expect("replace all");
    let (after, body) = last_modified_and_body(port).await;
    assert_eq!(after, stamp, "the content did not change");
    assert_eq!(body, "a", "a bulk replace still restarts the cycle");
    manager.delete_all().await;
}

/// The negative: changed content still moves the stamp, on every replace door.
#[tokio::test]
async fn replace_with_changed_content_moves_last_modified() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "stubs": [cycling_stub("s", "a")] }),
    )
    .await;
    let (stamp, _) = last_modified_and_body(port).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    manager
        .replace_stub(port, 0, stub(cycling_stub("s", "a2")))
        .await
        .expect("replace");
    let (after_one, _) = last_modified_and_body(port).await;
    assert_ne!(after_one, stamp, "replace_stub with new content");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    manager
        .replace_stubs(port, vec![stub(cycling_stub("s", "a3"))])
        .await
        .expect("replace all");
    let (after_all, _) = last_modified_and_body(port).await;
    assert_ne!(after_all, after_one, "replace_stubs with new content");
    manager.delete_all().await;
}
