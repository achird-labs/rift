//! Stage-1 path-anchor prefilter for imposter stub matching (issue #292).
//!
//! The imposter match loop (`core/matching.rs`) is a linear scan that runs full Mountebank
//! predicate evaluation on every stub. This index narrows that to a candidate set by the request
//! path, so only stubs that *could* match are evaluated — turning O(stubs) into ~O(1)–O(k) for the
//! common method/path-anchored cases.
//!
//! **Correctness is by construction.** A stub is path-anchored (indexed) only when a *top-level*
//! (implicitly AND-ed) predicate is a straightforward `equals`/`startsWith`/`contains` on the raw
//! `path` field — no selector, no `except`, not case-sensitive. Such a predicate is *required* for
//! the stub to match, so a request whose path fails it can never match the stub → safe to exclude.
//! Every other stub (regex/exists/body/method-only predicates, `or`/`not`/`and`, selectors,
//! `except`, `caseSensitive`, empty predicates) goes in the always-candidate `fallback` bucket.
//! `candidates()` therefore returns a *superset* of the true matches, in ascending stub order, so
//! Stage-2 evaluation preserves Mountebank first-match-wins exactly. The differential test below
//! (indexed candidates ≡ linear scan over a diverse corpus) is the guardrail.

use super::StubState;
use crate::imposter::types::Stub;
use rift_types::predicate::{Predicate, PredicateOperation};
use std::collections::HashMap;
use std::sync::Arc;

/// A required path constraint extracted from a stub's top-level predicates.
enum PathAnchor {
    Exact(String),
    Prefix(String),
    Contains(String),
}

/// The lowercased `path` value of a predicate's field map, if present and a string.
fn field_path(fields: &HashMap<String, serde_json::Value>) -> Option<String> {
    match fields.get("path") {
        Some(serde_json::Value::String(s)) => Some(s.to_lowercase()),
        _ => None,
    }
}

/// A single predicate's path anchor, if it is a safely-indexable required path constraint.
fn path_anchor(pred: &Predicate) -> Option<PathAnchor> {
    let p = &pred.parameters;
    // Anything that transforms or re-scopes the compared value can't be indexed by the raw path.
    // caseSensitive is opt-in; the default (None/Some(false)) is case-insensitive, so we lowercase.
    if p.case_sensitive == Some(true) || !p.except.is_empty() || p.selector.is_some() {
        return None;
    }
    match &pred.operation {
        PredicateOperation::Equals(fields) => field_path(fields).map(PathAnchor::Exact),
        PredicateOperation::StartsWith(fields) => field_path(fields).map(PathAnchor::Prefix),
        PredicateOperation::Contains(fields) => field_path(fields).map(PathAnchor::Contains),
        _ => None,
    }
}

/// The first required path anchor among a stub's top-level (AND-ed) predicates, or `None` if the
/// stub can't be safely path-indexed (→ fallback bucket).
fn classify(stub: &Stub) -> Option<PathAnchor> {
    stub.predicates.iter().find_map(path_anchor)
}

/// Path-anchor index over a specific stub snapshot. Embeds the exact `Arc` it describes so a reader
/// that loads the index gets a self-consistent (stubs, candidates) pair.
pub(crate) struct StubIndex {
    stubs: Arc<Vec<Arc<StubState>>>,
    exact: HashMap<String, Vec<usize>>,
    prefix: Vec<(String, Vec<usize>)>,
    contains: Vec<(String, Vec<usize>)>,
    fallback: Vec<usize>,
    /// Whether any stub's predicate tree contains an `inject` predicate, anywhere (including
    /// nested under `and`/`or`/`not`). Computed once per snapshot so the request hot path can
    /// gate the bounded (spawn_blocking) matching route on it for free (issue #476).
    has_inject: bool,
}

/// Does this predicate tree contain an `inject` predicate anywhere?
fn predicate_contains_inject(pred: &Predicate) -> bool {
    match &pred.operation {
        PredicateOperation::Inject(_) => true,
        PredicateOperation::Not(inner) => predicate_contains_inject(inner),
        PredicateOperation::And(children) | PredicateOperation::Or(children) => {
            children.iter().any(predicate_contains_inject)
        }
        _ => false,
    }
}

impl StubIndex {
    /// Classify every stub in `stubs` into its path-anchor bucket (or fallback), preserving
    /// ascending stub index within each bucket.
    pub(crate) fn build(stubs: Arc<Vec<Arc<StubState>>>) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut prefix: HashMap<String, Vec<usize>> = HashMap::new();
        let mut contains: HashMap<String, Vec<usize>> = HashMap::new();
        let mut fallback: Vec<usize> = Vec::new();

        for (i, state) in stubs.iter().enumerate() {
            match classify(&state.stub) {
                Some(PathAnchor::Exact(k)) => exact.entry(k).or_default().push(i),
                Some(PathAnchor::Prefix(k)) => prefix.entry(k).or_default().push(i),
                Some(PathAnchor::Contains(k)) => contains.entry(k).or_default().push(i),
                None => fallback.push(i),
            }
        }

        let has_inject = stubs
            .iter()
            .any(|s| s.stub.predicates.iter().any(predicate_contains_inject));

        StubIndex {
            stubs,
            exact,
            prefix: prefix.into_iter().collect(),
            contains: contains.into_iter().collect(),
            fallback,
            has_inject,
        }
    }

    /// Whether any stub in this snapshot uses an `inject` predicate (issue #476).
    pub(crate) fn has_inject(&self) -> bool {
        self.has_inject
    }

    /// The stub snapshot this index describes.
    pub(crate) fn stubs(&self) -> &Arc<Vec<Arc<StubState>>> {
        &self.stubs
    }

    /// Candidate stub indices for a request `path`, in ascending order (deduped). A superset of the
    /// stubs that could match — Stage-2 does the real Mountebank evaluation on these in order.
    pub(crate) fn candidates(&self, path: &str) -> Vec<usize> {
        let p = path.to_lowercase();
        let mut out: Vec<usize> = Vec::new();
        if let Some(v) = self.exact.get(&p) {
            out.extend_from_slice(v);
        }
        for (prefix, v) in &self.prefix {
            if p.starts_with(prefix.as_str()) {
                out.extend_from_slice(v);
            }
        }
        for (sub, v) in &self.contains {
            if p.contains(sub.as_str()) {
                out.extend_from_slice(v);
            }
        }
        out.extend_from_slice(&self.fallback);
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::core::Imposter;
    use crate::imposter::types::ImposterConfig;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn stub_states(preds: &[Value]) -> Arc<Vec<Arc<StubState>>> {
        let states: Vec<Arc<StubState>> = preds
            .iter()
            .map(|p| {
                let stub = serde_json::from_value(
                    json!({ "predicates": p, "responses": [{ "is": { "statusCode": 200 } }] }),
                )
                .expect("valid stub");
                Arc::new(StubState::new(stub))
            })
            .collect();
        Arc::new(states)
    }

    fn imposter(preds: &[Value]) -> Imposter {
        let stubs: Vec<Value> = preds
            .iter()
            .map(|p| json!({ "predicates": p, "responses": [{ "is": { "statusCode": 200 } }] }))
            .collect();
        let config: ImposterConfig =
            serde_json::from_value(json!({ "port": 9999, "protocol": "http", "stubs": stubs }))
                .expect("valid imposter config");
        Imposter::new(config).expect("test imposter")
    }

    /// A diverse corpus exercising every anchor category AND every fallback category, in an order
    /// that makes first-match-wins meaningful (the `not`/empty stubs at the end catch anything).
    fn corpus() -> Vec<Value> {
        vec![
            json!([{"equals": {"path": "/exact"}}]),       // 0 exact
            json!([{"equals": {"path": "/EXACT"}}]),       // 1 exact, other case
            json!([{"startsWith": {"path": "/pre"}}]),     // 2 prefix
            json!([{"contains": {"path": "mid"}}]),        // 3 contains
            json!([{"matches": {"path": "^/re[0-9]+$"}}]), // 4 regex -> fallback
            json!([{"exists": {"query": true}}]),          // 5 exists -> fallback
            json!([{"equals": {"method": "POST"}}]),       // 6 method-only -> fallback
            json!([{"equals": {"body": "ping"}}]),         // 7 body -> fallback
            json!([{"or": [{"equals": {"path": "/o1"}}, {"equals": {"path": "/o2"}}]}]), // 8 or -> fallback
            json!([{"not": {"equals": {"path": "/nope"}}}]), // 9 not -> fallback
            json!([{"equals": {"path": "/cs"}, "caseSensitive": true}]), // 10 caseSensitive -> fallback
            json!([{"equals": {"method": "GET", "path": "/mp"}}]),       // 11 method+path exact
            json!([]),                                                   // 12 match-all -> fallback
        ]
    }

    fn idx(r: anyhow::Result<Option<(Arc<StubState>, usize)>>) -> Option<usize> {
        r.expect("no backend error").map(|(_, i)| i)
    }

    // AC2: the indexed path returns the SAME matched stub as the linear scan for every request —
    // the correctness guardrail. Covers case-insensitivity, prefix/contains, all fallback
    // categories, method+path, and first-match-wins ordering (the trailing not/empty stubs).
    #[test]
    fn indexed_matching_equals_linear() {
        let imp = imposter(&corpus());
        let no_headers: HashMap<String, String> = HashMap::new();

        // (method, path, query, body)
        let requests: &[(&str, &str, Option<&str>, Option<&str>)] = &[
            ("GET", "/exact", None, None),
            ("GET", "/EXACT", None, None),
            ("GET", "/eXaCt", None, None), // case-insensitive collides on both 0 and 1 -> 0 wins
            ("GET", "/prefixed/deep", None, None),
            ("GET", "/pre", None, None),
            ("GET", "/x-mid-y", None, None),
            ("GET", "/re12", None, None),
            ("GET", "/re", None, None), // regex requires digits -> no 4; falls to not(9)
            ("GET", "/mp", None, None),
            ("POST", "/mp", None, None), // method+path requires GET -> not 11; POST hits 6
            ("GET", "/nope", None, None), // not(/nope) excludes -> empty(12)
            ("GET", "/cs", None, None),  // caseSensitive lives in fallback
            ("GET", "/CS", None, None),
            ("GET", "/o1", None, None),
            ("GET", "/o2", None, None),
            ("GET", "/anything", Some("a=1"), None), // exists{query} -> 5
            ("GET", "/anything", None, Some("ping")), // body -> 7 (9 not also matches, order)
            ("GET", "/zzz", None, None),             // nothing anchored -> not(9)
            ("POST", "/zzz", None, None),
            ("GET", "/pre-mid-exact", None, None), // matches prefix(2) AND contains(3): first wins
        ];

        for (m, p, q, b) in requests {
            let linear = idx(imp.find_matching_stub_linear(m, p, &no_headers, *q, *b, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, *q, *b, None, None));
            assert_eq!(
                indexed, linear,
                "index diverged from linear for {m} {p} q={q:?} b={b:?}"
            );
        }
    }

    // AC2 edge cases: the fold/normalization boundary where the index (Unicode `to_lowercase`) and
    // the `equals` evaluator (ASCII `eq_ignore_ascii_case`) differ, plus a path predicate nested in
    // `and` (must be fallback), multiple path predicates, and a trailing slash. No greedy `not` stub
    // here, so anchored stubs are actually reached and the boundary is exercised, not shadowed.
    #[test]
    fn indexed_matching_equals_linear_edge_cases() {
        let imp = imposter(&[
            json!([{"equals": {"path": "/café"}}]),  // 0 unicode exact
            json!([{"startsWith": {"path": "/A"}}]), // 1 prefix, uppercase anchor
            json!([{"and": [{"equals": {"method": "GET"}}, {"equals": {"path": "/andp"}}]}]), // 2 and -> fallback
            json!([{"equals": {"path": "/exact"}}, {"startsWith": {"path": "/exa"}}]), // 3 two path preds
            json!([{"contains": {"path": "/seg"}}]),                                   // 4 contains
            json!([{"equals": {"path": "/pm2"}}, {"equals": {"method": "GET"}}]), // 5 path anchor + separate method predicate
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        let requests: &[(&str, &str)] = &[
            ("GET", "/café"),
            ("GET", "/CAFÉ"), // ASCII fold: É != é so equals rejects; index over-includes harmlessly
            ("GET", "/caFé"),
            ("GET", "/a1"),   // startsWith /A, case-insensitive
            ("GET", "/andp"), // and-nested path lives in fallback (stub 2)
            ("POST", "/andp"),
            ("GET", "/exact"), // stub 3: both path preds hold
            ("GET", "/exa"),   // startsWith /exa holds but equals /exact fails -> not stub 3
            ("GET", "/x/seg/y"),
            ("GET", "/exact/"), // trailing slash is not equal to /exact
            ("GET", "/andp/extra"),
            ("GET", "/pm2"), // stub 5: path anchor indexes it, separate method predicate holds
            ("POST", "/pm2"), // path-anchored candidate, but Stage-2 method predicate rejects -> None
        ];
        for (m, p) in requests {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }
    }

    // AC1: the index genuinely narrows (excludes non-matching anchored stubs) yet never drops a
    // stub the linear scan would consider (fallback + matching anchors are all present).
    #[test]
    fn stub_index_narrows_and_covers() {
        let stubs = stub_states(&corpus());
        let index = StubIndex::build(Arc::clone(&stubs));
        let cands = index.candidates("/exact");

        // Narrowing: the prefix (2) and method+path-/mp (11) anchored stubs cannot match /exact,
        // so they are excluded.
        assert!(!cands.contains(&2), "prefix /pre stub excluded for /exact");
        assert!(
            !cands.contains(&11),
            "method+path /mp stub excluded for /exact"
        );

        // Coverage: both exact stubs (case-insensitive collision) and every fallback stub remain.
        assert!(
            cands.contains(&0) && cands.contains(&1),
            "exact stubs present"
        );
        for fb in [4, 5, 6, 7, 8, 9, 10, 12] {
            assert!(
                cands.contains(&fb),
                "fallback stub {fb} must always be a candidate"
            );
        }
        // Ascending + deduped so Stage-2 preserves declaration order.
        let mut sorted = cands.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(cands, sorted, "candidates must be ascending and deduped");
    }

    // AC3: rebuilding on stub reload keeps the index consistent with the new stubs.
    #[test]
    fn index_rebuilt_on_replace_stubs() {
        let imp = imposter(&[json!([{"equals": {"path": "/old"}}])]);
        let no_headers: HashMap<String, String> = HashMap::new();
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "GET",
                "/old",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            Some(0)
        );

        let new_stub =
            serde_json::from_value(json!({ "predicates": [{"equals": {"path": "/new"}}], "responses": [{ "is": { "statusCode": 200 } }] }))
                .expect("valid stub");
        imp.replace_stubs(vec![new_stub]);

        // Old path no longer matches; new path does — proves the index was rebuilt, not stale.
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "GET",
                "/old",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            None
        );
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "GET",
                "/new",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            Some(0)
        );
    }

    // AC2: a match-all (empty-predicate) stub declared BEFORE an anchored stub must still win —
    // the index (fallback, low index) can never let a higher-index anchor jump declaration order.
    #[test]
    fn match_all_before_anchor_wins() {
        let imp = imposter(&[
            json!([]),                           // 0 match-all (fallback)
            json!([{"equals": {"path": "/a"}}]), // 1 exact anchor
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        // /a matches both; the earlier match-all (stub 0) wins in both the indexed and linear paths.
        assert_eq!(
            idx(imp.find_matching_stub_linear("GET", "/a", &no_headers, None, None, None, None)),
            Some(0)
        );
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "GET",
                "/a",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            Some(0),
            "the earlier match-all stub must win over the anchored stub"
        );
    }

    // Issue #476: the has_inject gate — computed once at index build — detects an inject
    // predicate anywhere in a stub's predicate tree, including nested under and/or/not, and
    // stays false for scriptless stub sets so they keep the inline matching fast path.
    #[test]
    fn has_inject_detects_top_level_and_nested() {
        let scriptless = StubIndex::build(stub_states(&[
            json!([{"equals": {"path": "/a"}}]),
            json!([{"and": [{"equals": {"path": "/b"}}, {"exists": {"query": {"q": true}}}]}]),
        ]));
        assert!(!scriptless.has_inject());

        let top_level = StubIndex::build(stub_states(&[
            json!([{"equals": {"path": "/a"}}]),
            json!([{"inject": "function (config) { return true; }"}]),
        ]));
        assert!(top_level.has_inject());

        let under_and = StubIndex::build(stub_states(&[json!([
            {"and": [{"equals": {"path": "/a"}}, {"inject": "function (config) { return true; }"}]}
        ])]));
        assert!(under_and.has_inject());

        let under_not_in_or = StubIndex::build(stub_states(&[json!([
            {"or": [{"equals": {"path": "/a"}}, {"not": {"inject": "function (config) { return true; }"}}]}
        ])]));
        assert!(under_not_in_or.has_inject());
    }
}
