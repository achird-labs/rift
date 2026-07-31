//! Match outcomes recorded alongside journal entries: an operator reading the request journal
//! must be able to answer "why did this request not match?" without re-deriving the match by hand.
//!
//! These tests drive the real imposter serve loop end-to-end (`ImposterManager` + reqwest, the
//! same pattern as `issue_636_binary_request_bodies.rs`) and assert the WIRE shape of
//! `matchOutcome` on the recorded entry, because that projection is what an operator (and the
//! admin API's `savedRequests`) actually sees.
//!
//! What is pinned here:
//!   - a miss names every candidate the matcher VISITED, in visit order, with the index of the
//!     predicate that rejected it
//!   - a hit names the winner and only the candidates visited BEFORE it
//!   - the two eligibility gates (space, scenario state) are reported as skips, not as failures
//!   - the `tried` list is capped, with the overflow counted rather than silently dropped
//!   - a `RetryMatch` rescue attaches the RESCUED outcome, not the first pass's miss
//!   - `recordRequests: false` records nothing and attaches nothing

use rift_mock_core::extensions::no_match::{NoMatchContext, NoMatchDirective, NoMatchInterceptor};
use rift_mock_core::imposter::{ImposterManager, Stub};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

async fn create(manager: &ImposterManager, cfg: serde_json::Value) -> u16 {
    let config = serde_json::from_value(cfg).expect("valid imposter config");
    let port = manager.create_imposter(config).await.expect("create");
    // Give the listener a moment to bind before the request (matches the sibling HTTP tests).
    tokio::time::sleep(Duration::from_millis(150)).await;
    port
}

/// Drive one GET, optionally carrying an `X-Want` header (the field every fixture below
/// discriminates on — deliberately NOT path or method, so the Stage-1 index cannot prune any
/// stub and every stub is genuinely visited).
async fn get(port: u16, path: &str, want: Option<&str>) {
    let mut req = reqwest::Client::new().get(format!("http://127.0.0.1:{port}{path}"));
    if let Some(want) = want {
        req = req.header("X-Want", want);
    }
    req.send().await.expect("send");
}

/// The `matchOutcome` of the port's single journal entry, as the JSON an operator would read.
fn only_outcome(manager: &ImposterManager, port: u16) -> serde_json::Value {
    let imposter = manager.get_imposter(port).expect("imposter exists");
    let recorded = imposter.get_recorded_requests();
    assert_eq!(recorded.len(), 1, "exactly one request was recorded");
    let entry = serde_json::to_value(&recorded[0]).expect("serializes");
    entry
        .get("matchOutcome")
        .cloned()
        .unwrap_or_else(|| panic!("the journal entry must carry a matchOutcome: {entry}"))
}

// G1: a miss must name every candidate the matcher visited, in visit order, each with the index of
// the predicate that rejected it. The first stub's rejecting predicate is its SECOND one, so this
// also proves the index is the failing predicate's position and not merely always zero.
#[tokio::test]
async fn unmatched_request_records_each_failing_candidate_in_visit_order() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [
                { "id": "first",
                  "predicates": [
                      { "equals": { "method": "GET" } },
                      { "equals": { "headers": { "X-Want": "a" } } }],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                { "id": "second",
                  "predicates": [{ "equals": { "headers": { "X-Want": "b" } } }],
                  "responses": [{ "is": { "statusCode": 200 } }] }
            ]
        }),
    )
    .await;

    get(port, "/x", Some("z")).await;

    assert_eq!(
        only_outcome(&manager, port),
        json!({
            "matched": false,
            "tried": [
                { "stubIndex": 0, "stubId": "first",
                  "why": { "reason": "failedPredicate", "predicateIndex": 1 } },
                { "stubIndex": 1, "stubId": "second",
                  "why": { "reason": "failedPredicate", "predicateIndex": 0 } }
            ]
        })
    );

    manager.delete_all().await;
}

// G2: a hit names the winner and only the candidates visited BEFORE it — the winner itself is not
// a "tried" entry, and nothing after it was ever evaluated (first-match-wins).
#[tokio::test]
async fn matched_request_records_the_winner_and_only_earlier_candidates() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [
                { "predicates": [{ "equals": { "headers": { "X-Want": "a" } } }],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                { "id": "second",
                  "predicates": [{ "equals": { "headers": { "X-Want": "b" } } }],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                { "id": "winner",
                  "predicates": [{ "equals": { "headers": { "X-Want": "c" } } }],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                { "id": "never-reached",
                  "predicates": [],
                  "responses": [{ "is": { "statusCode": 200 } }] }
            ]
        }),
    )
    .await;

    get(port, "/x", Some("c")).await;

    assert_eq!(
        only_outcome(&manager, port),
        json!({
            "matched": true,
            "stubIndex": 2,
            "stubId": "winner",
            "tried": [
                { "stubIndex": 0, "why": { "reason": "failedPredicate", "predicateIndex": 0 } },
                { "stubIndex": 1, "stubId": "second",
                  "why": { "reason": "failedPredicate", "predicateIndex": 0 } }
            ]
        })
    );

    manager.delete_all().await;
}

// G3: the two eligibility gates are reported as SKIPS with their own reasons. A gated stub never
// reaches predicate evaluation, so reporting it as a failed predicate would be a lie about why it
// did not participate.
#[tokio::test]
async fn gated_candidates_are_recorded_as_skips_with_their_reason() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http", "recordRequests": true,
            "stubs": [
                // Scoped to a space no request on this port can resolve to (the default
                // flowIdSource is the imposter port).
                { "space": "someone-else", "predicates": [],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                // Gated on a scenario state the FSM is not in (it starts at "Started").
                { "scenarioName": "s", "requiredScenarioState": "Done", "predicates": [],
                  "responses": [{ "is": { "statusCode": 200 } }] },
                { "id": "open", "predicates": [],
                  "responses": [{ "is": { "statusCode": 200 } }] }
            ]
        }),
    )
    .await;

    get(port, "/x", None).await;

    assert_eq!(
        only_outcome(&manager, port),
        json!({
            "matched": true,
            "stubIndex": 2,
            "stubId": "open",
            "tried": [
                { "stubIndex": 0, "why": { "reason": "skippedSpace" } },
                { "stubIndex": 1, "why": { "reason": "skippedScenarioState" } }
            ]
        })
    );

    manager.delete_all().await;
}

// G4: the journal ring holds 10k entries per port, so an outcome cannot grow without bound. Past
// the cap the list stops and the overflow is COUNTED — silently truncating would make "these are
// the stubs that were tried" false without saying so.
#[tokio::test]
async fn tried_list_is_capped_with_the_overflow_counted() {
    const CAP: usize = 25;
    const STUBS: usize = CAP + 5;

    let stubs: Vec<serde_json::Value> = (0..STUBS)
        .map(|i| {
            json!({
                "predicates": [{ "equals": { "headers": { "X-Want": format!("v{i}") } } }],
                "responses": [{ "is": { "statusCode": 200 } }]
            })
        })
        .collect();
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "recordRequests": true, "stubs": stubs }),
    )
    .await;

    get(port, "/x", Some("matches-nothing")).await;

    let outcome = only_outcome(&manager, port);
    assert_eq!(outcome["matched"], false);
    let tried = outcome["tried"].as_array().expect("tried is an array");
    assert_eq!(tried.len(), CAP, "exactly the cap is retained");
    assert_eq!(
        tried[0]["stubIndex"], 0,
        "retained from the START of the scan"
    );
    assert_eq!(tried[CAP - 1]["stubIndex"], CAP - 1);
    assert_eq!(
        outcome["triedOmitted"], 5,
        "the visited-but-unlisted candidates are counted, not dropped: {outcome}"
    );

    manager.delete_all().await;
}

/// A no-match interceptor that installs a matching stub and asks for one retry, standing in for
/// "the replicated config caught up" (the seam issue #819 added).
struct Rescuer {
    target: parking_lot::Mutex<Option<(Arc<ImposterManager>, u16)>>,
}

impl NoMatchInterceptor for Rescuer {
    fn on_no_match<'a>(
        &'a self,
        _ctx: NoMatchContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = NoMatchDirective> + Send + 'a>> {
        let target = self.target.lock().clone();
        Box::pin(async move {
            if let Some((manager, port)) = target {
                let stub: Stub = serde_json::from_value(json!({
                    "id": "late",
                    "predicates": [{ "equals": { "headers": { "X-Want": "late" } } }],
                    "responses": [{ "is": { "statusCode": 200, "body": "rescued" } }]
                }))
                .expect("late stub");
                manager.add_stub(port, stub, None).await.expect("add stub");
            }
            NoMatchDirective::RetryMatch
        })
    }
}

// G7: a rescued request is a match, so the attached outcome must be the RESCUED one. Attaching the
// first pass's miss would tell an operator the request went unmatched when it was in fact served.
#[tokio::test]
async fn retry_match_rescue_attaches_the_rescued_outcome() {
    let rescuer = Arc::new(Rescuer {
        target: parking_lot::Mutex::new(None),
    });
    let manager = Arc::new(
        ImposterManager::new()
            .with_no_match_interceptor(Arc::clone(&rescuer) as Arc<dyn NoMatchInterceptor>),
    );
    let port = create(
        &manager,
        json!({ "port": 0, "protocol": "http", "recordRequests": true, "stubs": [] }),
    )
    .await;
    // Armed after construction so the rescue mutates the manager actually serving the request.
    *rescuer.target.lock() = Some((Arc::clone(&manager), port));

    get(port, "/late", Some("late")).await;

    assert_eq!(
        only_outcome(&manager, port),
        json!({ "matched": true, "stubIndex": 0, "stubId": "late" })
    );

    manager.delete_all().await;
}

// G8: with recording off there is no entry to annotate, so nothing is recorded and nothing is
// attached — matching itself is untouched. Pins that the outcome capture never resurrects a
// journal the operator turned off.
#[tokio::test]
async fn recording_off_leaves_the_journal_empty_and_matching_intact() {
    let manager = ImposterManager::new();
    let port = create(
        &manager,
        json!({
            "port": 0, "protocol": "http", "recordRequests": false,
            "stubs": [
                { "predicates": [{ "equals": { "headers": { "X-Want": "a" } } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "no" } }] },
                { "predicates": [{ "equals": { "headers": { "X-Want": "b" } } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "yes" } }] }
            ]
        }),
    )
    .await;

    let body = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/x"))
        .header("X-Want", "b")
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "yes", "matching is unaffected by the outcome capture");

    let imposter = manager.get_imposter(port).expect("imposter exists");
    assert!(
        imposter.get_recorded_requests().is_empty(),
        "recordRequests: false records nothing at all"
    );
    assert_eq!(
        imposter.get_request_count(),
        1,
        "the request still counts toward numberOfRequests"
    );

    manager.delete_all().await;
}
