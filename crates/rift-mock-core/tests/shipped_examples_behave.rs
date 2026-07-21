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
