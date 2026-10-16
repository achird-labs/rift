//! Issue #805: a shipped example that *loads* is not the same as one that *behaves*.
//!
//! Matching is first-match-wins, so a general stub listed before a more specific one makes the
//! specific one dead code: the document lints clean, starts cleanly, and silently never serves
//! two of the behaviors it advertises. `shipped_examples_load.rs` cannot catch that — it never
//! asks the engine which stub a request would actually select. These tests do, by driving the
//! real matcher over the shipped file and asserting the *served response*, not just the load.

use rift_mock_core::imposter::{Imposter, ImposterConfig, StubResponse};
use std::collections::HashMap;
use std::path::PathBuf;

fn load_example(file: &str) -> Imposter {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(file);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
    let v: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{file} is not valid JSON: {e}"));
    let first = v
        .get("imposters")
        .and_then(|i| i.get(0))
        .unwrap_or_else(|| panic!("{file}: no imposters[0]"))
        .clone();
    let config: ImposterConfig =
        serde_json::from_value(first).unwrap_or_else(|e| panic!("{file}: imposter: {e}"));
    Imposter::new(config).unwrap_or_else(|e| panic!("{file}: imposter construction: {e}"))
}

/// The status code and body the engine would serve for `GET <path>?<query>`, or `None` when no
/// stub matches.
fn serve_get(
    imposter: &Imposter,
    path: &str,
    query: Option<&str>,
) -> Option<(u16, serde_json::Value)> {
    let headers: HashMap<String, String> = HashMap::new();
    let (stub, _idx) = imposter
        .find_matching_stub_with_client("GET", path, &headers, query, None, None, None)
        .expect("matching must not error")?;
    let response = stub.get_next_response().expect("stub has a response");
    match response {
        StubResponse::Is { is, .. } => Some((
            is.status_code,
            is.body.clone().unwrap_or(serde_json::Value::Null),
        )),
        other => panic!("expected an `is` response, got {other:?}"),
    }
}

const TASKS: &str = "task-management-api.json";

// AC1 — the `?status=OPEN` stub must not be shadowed by the bare `/tasks` GET listed before it.
#[test]
fn task_management_filter_by_status_is_reachable() {
    let imposter = load_example(TASKS);
    let (status, body) = serve_get(&imposter, "/tasks", Some("status=OPEN"))
        .expect("GET /tasks?status=OPEN matches");
    assert_eq!(status, 200);
    assert_eq!(
        body.get("count").and_then(serde_json::Value::as_u64),
        Some(1),
        "GET /tasks?status=OPEN must serve the filtered stub, not the full list: {body}"
    );
}

// AC2 — reordering must not cost the unfiltered listing: bare `/tasks` still lists everything.
#[test]
fn task_management_bare_tasks_lists_all() {
    let imposter = load_example(TASKS);
    let (status, body) = serve_get(&imposter, "/tasks", None).expect("GET /tasks matches");
    assert_eq!(status, 200);
    assert_eq!(
        body.get("count").and_then(serde_json::Value::as_u64),
        Some(3),
        "GET /tasks must still serve the full list: {body}"
    );
}

// AC3 — the 404 stub must not be shadowed by the `task-\d+` stub listed before it.
#[test]
fn task_management_unknown_task_is_not_found() {
    let imposter = load_example(TASKS);
    let (status, body) =
        serve_get(&imposter, "/tasks/task-999", None).expect("GET /tasks/task-999 matches");
    assert_eq!(
        status, 404,
        "GET /tasks/task-999 must serve the not-found stub: {body}"
    );
    assert_eq!(
        body.get("code").and_then(serde_json::Value::as_str),
        Some("TASK_NOT_FOUND")
    );
}

// `matches` is a substring search, so the newly-reachable not-found stub would otherwise also claim
// `task-9990`, `task-9991`, … — a behaviour the reorder would have introduced by accident. Its path
// is anchored; this pins that only the literal id 999 is the unknown one.
#[test]
fn task_management_not_found_stub_does_not_over_match() {
    let imposter = load_example(TASKS);
    let (status, body) =
        serve_get(&imposter, "/tasks/task-9990", None).expect("GET /tasks/task-9990 matches");
    assert_eq!(
        status, 200,
        "only /tasks/task-999 is the unknown id; task-9990 must still serve a task: {body}"
    );
}

// AC4 — reordering must not cost the happy path: a real task id still resolves.
#[test]
fn task_management_known_task_is_returned() {
    let imposter = load_example(TASKS);
    let (status, body) =
        serve_get(&imposter, "/tasks/task-001", None).expect("GET /tasks/task-001 matches");
    assert_eq!(status, 200);
    assert_eq!(
        body.get("taskId").and_then(serde_json::Value::as_str),
        Some("task-001"),
        "GET /tasks/task-001 must serve the task body: {body}"
    );
}

// ===========================================================================
// Issue #1492 (ported from rift-conformance's ShippedExamples120 spec): every shipped example is
// served through a real listener and its headline behaviour driven, asserting the served body (so
// a wrong-stub match cannot pass on status alone) and, for latency-testing, the elapsed time —
// rift#608 shipped `_behaviors` silently dropped while the document still loaded cleanly.
// ===========================================================================

use rift_mock_core::imposter::ImposterManager;
use std::time::{Duration, Instant};

/// One driven request and what the example promises it answers.
struct Drive {
    method: &'static str,
    path: &'static str,
    status: u16,
    body_contains: &'static str,
    body: Option<&'static str>,
    header: Option<(&'static str, &'static str)>,
    min_elapsed: Duration,
    expect_no_match: bool,
}

const fn get(path: &'static str, status: u16, body_contains: &'static str) -> Drive {
    Drive {
        method: "GET",
        path,
        status,
        body_contains,
        body: None,
        header: None,
        min_elapsed: Duration::ZERO,
        expect_no_match: false,
    }
}

const fn send_json(
    method: &'static str,
    path: &'static str,
    body: &'static str,
    status: u16,
    body_contains: &'static str,
) -> Drive {
    Drive {
        body: Some(body),
        header: Some(("content-type", "application/json")),
        method,
        ..get(path, status, body_contains)
    }
}

/// Load the example's first imposter verbatim except its `port`, which is auto-assigned so the
/// test never collides with a concurrent run (#1461); the document's own port is irrelevant to
/// what its stubs serve.
async fn serve_example(manager: &ImposterManager, file: &str) -> u16 {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(file);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
    let mut doc: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{file} is not valid JSON: {e}"));
    let mut imposter = doc["imposters"][0].take();
    assert!(
        imposter["port"].is_u64(),
        "{file}: the shipped example declares its port"
    );
    imposter
        .as_object_mut()
        .expect("imposter object")
        .remove("port");
    let config: ImposterConfig =
        serde_json::from_value(imposter).unwrap_or_else(|e| panic!("{file}: imposter: {e}"));
    manager
        .create_imposter(config)
        .await
        .unwrap_or_else(|e| panic!("{file}: create: {e}"))
}

async fn drive_example(file: &str, drives: &[Drive]) {
    let manager = ImposterManager::new();
    let port = serve_example(&manager, file).await;
    let client = reqwest::Client::new();
    for d in drives {
        let method = reqwest::Method::from_bytes(d.method.as_bytes()).expect("method");
        let mut req = client.request(method, format!("http://127.0.0.1:{port}{}", d.path));
        if let Some((k, v)) = d.header {
            req = req.header(k, v);
        }
        if let Some(b) = d.body {
            req = req.body(b);
        }
        let started = Instant::now();
        let resp = req.send().await.expect("send");
        let no_match = resp.headers().contains_key("x-rift-no-match");
        let status = resp.status().as_u16();
        let body = resp.text().await.expect("body");
        let elapsed = started.elapsed();
        let at = format!("{file} {} {}", d.method, d.path);
        assert_eq!(no_match, d.expect_no_match, "{at}: x-rift-no-match");
        assert_eq!(status, d.status, "{at}: status (body {body})");
        assert!(
            body.contains(d.body_contains),
            "{at}: body does not contain {:?}: {body}",
            d.body_contains
        );
        assert!(
            elapsed >= d.min_elapsed,
            "{at}: answered in {elapsed:?}, expected at least {:?}",
            d.min_elapsed
        );
    }
    manager.delete_imposter(port).await.expect("delete");
}

/// The drive table is hand-curated, so a newly shipped example nobody adds drives for would
/// lint clean and never be served here; this pins the set.
#[test]
fn every_shipped_example_has_a_served_drive() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("examples dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk,
        [
            "authentication-api.json",
            "basic-api.json",
            "error-testing.json",
            "feature-flags-api.json",
            "latency-testing.json",
            "task-management-api.json",
        ]
    );
}

#[tokio::test]
async fn basic_api_example_serves_its_bodies() {
    drive_example(
        "basic-api.json",
        &[
            // Plain-text "OK", unique among this document's bodies.
            get("/health", 200, "OK"),
            get("/api/users", 200, "Bob"),
            // The `and`+`matches` regex stub, not the bare collection above it.
            get("/api/users/42", 200, "alice@example.com"),
            send_json("POST", "/api/users", r#"{"name":"Zed"}"#, 201, "Created"),
            // Negative control: the no-match header IS still emitted on a genuine miss, so the
            // `x-rift-no-match` guard on every other drive is not inert.
            Drive {
                expect_no_match: true,
                ..get("/no-such-path", 200, "")
            },
        ],
    )
    .await;
}

#[tokio::test]
async fn error_testing_example_serves_each_error() {
    drive_example(
        "error-testing.json",
        &[
            get("/success", 200, "\"ok\""),
            get("/error/400", 400, "INVALID_INPUT"),
            get("/error/401", 401, "Unauthorized"),
            get("/error/403", 403, "Forbidden"),
            get("/error/404", 404, "Not Found"),
            get("/error/429", 429, "Too Many Requests"),
            get("/error/500", 500, "Internal Server Error"),
            get("/error/502", 502, "Bad Gateway"),
            get("/error/503", 503, "Service Unavailable"),
            get("/error/504", 504, "Gateway Timeout"),
        ],
    )
    .await;
}

#[tokio::test]
async fn authentication_api_example_discriminates_credentials_and_tokens() {
    drive_example(
        "authentication-api.json",
        &[
            send_json(
                "POST",
                "/auth/login",
                r#"{"username":"admin","password":"secret123"}"#,
                200,
                "Bearer",
            ),
            // `contains` username AND `not`/`equals` password: the negated-predicate path.
            send_json(
                "POST",
                "/auth/login",
                r#"{"username":"admin","password":"wrong"}"#,
                401,
                "AUTH_INVALID_CREDENTIALS",
            ),
            Drive {
                header: Some(("authorization", "Bearer abc")),
                ..get("/auth/validate", 200, "\"valid\"")
            },
            // Same path without the header: `not`/`exists` on headers flips the answer.
            get("/auth/validate", 401, "AUTH_MISSING_TOKEN"),
        ],
    )
    .await;
}

#[tokio::test]
async fn feature_flags_api_example_serves_each_flag() {
    drive_example(
        "feature-flags-api.json",
        &[
            // Matched on the per-flag description, not the id: the collection body carries every
            // id, so an id would still pass if path discrimination broke.
            get("/features/DARK_MODE", 200, "Enable dark mode UI theme"),
            get("/features/BETA_FEATURE", 200, "New experimental feature"),
            get("/features/UNKNOWN_FOO", 404, "not found"),
            get("/features", 200, "\"features\""),
        ],
    )
    .await;
}

#[tokio::test]
async fn task_management_api_example_serves_through_a_listener() {
    drive_example(
        "task-management-api.json",
        &[
            get("/tasks", 200, "\"count\":3"),
            get("/tasks/task-001", 200, "alice@example.com"),
            get("/tasks?status=OPEN", 200, "\"count\":1"),
            get("/tasks/task-999", 404, "TASK_NOT_FOUND"),
            // jsonpath `$.name` present vs absent: the 201/400 split is the predicate working.
            send_json("POST", "/tasks", r#"{"name":"New"}"#, 201, "task-new-001"),
            send_json("POST", "/tasks", r#"{"nope":"x"}"#, 400, "VALIDATION_ERROR"),
            send_json(
                "PUT",
                "/tasks/task-001",
                r#"{"name":"U"}"#,
                200,
                "Task updated successfully",
            ),
            Drive {
                method: "DELETE",
                ..get("/tasks/task-001", 204, "")
            },
        ],
    )
    .await;
}

/// The rift#608 regression: latency-testing's `wait` behaviours must actually delay the answer,
/// not merely survive loading. `/timeout` (35 s) and `/slow-2s` prove nothing the 100/500 ms
/// floors don't.
#[tokio::test]
async fn latency_testing_example_actually_delays() {
    let mut drives = vec![
        Drive {
            min_elapsed: Duration::from_millis(100),
            ..get("/slow-100ms", 200, "100ms delay")
        },
        Drive {
            min_elapsed: Duration::from_millis(500),
            ..get("/slow-500ms", 200, "500ms delay")
        },
    ];
    // `{"wait": {"inject": …}}`, the exact shape rift#608 dropped; its floor is the function's
    // own minimum.
    if cfg!(feature = "javascript") {
        drives.push(Drive {
            min_elapsed: Duration::from_millis(100),
            ..get("/random-latency", 200, "Random latency")
        });
    }
    drive_example("latency-testing.json", &drives).await;
}
